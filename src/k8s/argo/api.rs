//! Argo CD read by its own API, with no kubeconfig for the cluster it runs in.
//!
//! Its servers are those the argocd CLI is logged in to (its config file: contexts, servers
//! and their users' tokens) and those added in Settings → Argo CD (an address, and a token
//! kept in `argocd-api-tokens.json`, readable only by the user). A token is never shown: it
//! prints as `[token]` and errors are scrubbed of it. It is sent to that server only, over
//! https (plain http only to this machine), and redirects are not followed.
//!
//! The Applications of a server are listed once (`GET /api/v1/applications`, with `fields`
//! so only what the page shows comes back), then kept live from Argo CD's application stream
//! (`/api/v1/stream/applications`, server-sent events). A stream that ends or fails is opened
//! again after a backoff (1 s, doubling to 60 s), listing again first: nothing is polled. A
//! server's feed runs while a window shows the Argo CD page ([`watch`]), shared by them all.
//!
//! The actions of the Argo CD page on an Application read this way (refresh, sync,
//! terminate, roll back) go through the same API ([`act`]), refused while write mode is off.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use serde_json::{Map, Value, json};

use super::server::{self, Login, Server, encode, scrub};
use super::windows::{self, SyncWindow};
use super::{DEFAULT_NAMESPACE, Summary, TRACKING_ID};

/// Where the tokens set in Settings → Argo CD are kept, by server address (mode 0600).
const TOKENS_FILE: &str = "argocd-api-tokens.json";
/// The longest wait for a connection.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
/// A stream silent this long is opened again (a connection that died without a word).
const IDLE: Duration = Duration::from_secs(15 * 60);
/// The first wait before opening a stream again, doubled each time up to `MAX_BACKOFF`.
const BACKOFF: Duration = Duration::from_secs(1);
const MAX_BACKOFF: Duration = Duration::from_secs(60);
/// A stream that stayed open this long starts its backoff over.
const STEADY: Duration = Duration::from_secs(60);
/// The largest event read from the stream.
const MAX_EVENT: usize = 32 * 1024 * 1024;
/// Deployments of an Application's history kept (the page shows 5).
const HISTORY: usize = 5;
/// Projects asked one by one for their sync windows, at most, when they can't be listed.
const MAX_PROJECTS: usize = 50;
/// How often the projects' sync windows are read again while a feed follows its stream.
const PROJECTS_EVERY: Duration = Duration::from_secs(5 * 60);

/// The fields of an Application the page reads; the rest is not sent (`fields`).
const APP_FIELDS: [&str; 27] = [
    "metadata.name",
    "metadata.namespace",
    "metadata.creationTimestamp",
    "metadata.labels",
    "metadata.annotations",
    "metadata.deletionTimestamp",
    "metadata.resourceVersion",
    "spec.project",
    "spec.destination",
    "spec.syncPolicy",
    "operation",
    "status.sync",
    "status.health",
    "status.operationState.phase",
    "status.operationState.message",
    "status.operationState.startedAt",
    "status.operationState.finishedAt",
    // For the sync journal: who started it, how, at which revision, and each resource's result.
    "status.operationState.operation.initiatedBy",
    "status.operationState.operation.sync.dryRun",
    "status.operationState.operation.sync.syncStrategy",
    "status.operationState.syncResult.revision",
    "status.operationState.syncResult.revisions",
    "status.operationState.syncResult.resources",
    "status.history",
    "status.conditions",
    "status.summary",
    "status.resourceHealthSource",
];

// MARK: Servers

/// A token, which never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[token]")
    }
}

/// An Argo CD API server to read, and how to log in to it.
#[derive(Clone, Debug)]
pub struct Endpoint {
    pub server: Server,
    /// The argocd CLI's context for it, when the CLI is logged in to it.
    pub cli_context: Option<String>,
    /// Added in Settings → Argo CD.
    pub in_settings: bool,
    /// Its certificate is not checked (the CLI's `--insecure`).
    pub insecure: bool,
    /// The token and where it comes from, or why there is none.
    pub login: Result<(Token, Login), String>,
    /// Read on the Argo CD page (Settings → Argo CD turns it off).
    pub on: bool,
    /// A token is set for it in Settings, expired or not (it can be forgotten).
    pub settings_token: bool,
}

impl Endpoint {
    /// `argocd.example.com`: how it is named on the page.
    pub fn name(&self) -> String {
        self.server.name()
    }

    /// The same Argo CD as a hub whose server is this one (see `grouping::hub_instance`).
    pub fn instance(&self) -> String {
        super::grouping::server_instance(&self.server.base)
    }

    /// The Application's page in Argo CD's web UI.
    pub fn web_url(&self, namespace: &str, name: &str) -> String {
        format!("{}/applications/{}/{}", self.server.base, encode(namespace), encode(name))
    }

    fn conn(&self) -> Result<Conn, String> {
        let (token, _) = self.login.clone()?;
        Ok(Conn { server: self.server.clone(), token, insecure: self.insecure })
    }

    /// What its feed is kept under: its address, and a hash of its login and settings, so a
    /// new login (`argocd login` again, a token set) starts a new feed. Never the token.
    fn feed_key(&self) -> String {
        use std::hash::{Hash as _, Hasher as _};
        let mut h = std::collections::hash_map::DefaultHasher::new();
        match &self.login {
            Ok((t, how)) => (&t.0, how.text()).hash(&mut h),
            Err(e) => e.hash(&mut h),
        }
        self.insecure.hash(&mut h);
        format!("{}#{:x}", self.server.base, h.finish())
    }
}

/// The servers to read, and why some of the config could not be used.
#[derive(Clone, Debug, Default)]
pub struct Endpoints {
    pub list: Vec<Endpoint>,
    pub errors: Vec<String>,
    /// CLI contexts that can't be used here, and how to connect instead: said quietly in
    /// Settings → Argo CD, not as an error on the Argo CD page.
    pub notes: Vec<String>,
}

/// A server of the argocd CLI's config, as read (not `Debug`: it holds the token).
struct CliEntry {
    context: String,
    current: bool,
    server: Result<Server, String>,
    insecure: bool,
    token: Option<String>,
}

/// The server name `argocd login --port-forward` saves: the CLI port-forwards to argocd-server
/// itself each time, through the kubeconfig. There is no address to ask.
const CLI_PORT_FORWARD: &str = "port-forward";

/// The API servers of the argocd CLI's config `text`, one entry per context (`--core`
/// contexts have none; a `--port-forward` one has none either, and a line in `notes` says
/// so). A config that doesn't read gives nothing, and says why.
fn cli_entries(text: &str, notes: &mut Vec<String>) -> Result<Vec<CliEntry>, String> {
    if text.trim().is_empty() {
        return Ok(Vec::new());
    }
    let config: server::CliConfig =
        crate::yaml::from_str(text).map_err(|e| format!("the argocd CLI's config doesn't read: {e}"))?;
    let mut out = Vec::new();
    for c in &config.contexts {
        let Some(s) = config.servers.iter().find(|s| s.server == c.server) else { continue };
        if s.core || s.server.trim().is_empty() {
            continue;
        }
        if s.server.trim() == CLI_PORT_FORWARD {
            notes.push(format!(
                "argocd CLI context {} uses the CLI's port-forward mode, which can't be used here: log in to the \
                 server's address instead (`argocd login <address>`), or port-forward to argocd-server yourself \
                 and add http://localhost:<port> here",
                c.name
            ));
            continue;
        }
        let scheme = if s.plain_text { "http" } else { "https" };
        let root = s.grpc_web_root_path.trim().trim_matches('/');
        let url = if root.is_empty() {
            format!("{scheme}://{}", s.server.trim())
        } else {
            format!("{scheme}://{}/{root}", s.server.trim())
        };
        let server = Server::parse(&url).map_err(|e| format!("argocd CLI context {}: {e}", c.name));
        let token = config
            .users
            .iter()
            .find(|u| u.name == c.user)
            .map(|u| u.auth_token.trim().to_string())
            .filter(|t| !t.is_empty());
        out.push(CliEntry {
            context: c.name.clone(),
            current: c.name == config.current_context,
            server,
            insecure: s.insecure,
            token,
        });
    }
    Ok(out)
}

/// What to log in with: the token set in Settings, else the CLI's; and whether it is
/// expired at `now` (seconds).
fn login_of(settings: Option<&str>, cli: Option<&str>, name: &str, now: i64) -> Result<(Token, Login), String> {
    let relogin = format!("run `argocd login {name}` (with `--sso` for SSO) or set a token in Settings → Argo CD");
    match (settings.filter(|t| !t.trim().is_empty()), cli.filter(|t| !t.trim().is_empty())) {
        (Some(t), _) if server::expired(t, now) => {
            Err(format!("The token set in Settings for {name} has expired: set a new one"))
        }
        (Some(t), _) => Ok((Token(t.trim().to_string()), Login::Settings)),
        (None, Some(t)) if server::expired(t, now) => {
            Err(format!("The argocd CLI's login to {name} has expired: {relogin}"))
        }
        (None, Some(t)) => Ok((Token(t.trim().to_string()), Login::Cli)),
        (None, None) => Err(format!("No login to {name}: {relogin}")),
    }
}

/// The servers from the CLI's config (`cli`: its text, or why it couldn't be read; None:
/// there is none), the addresses set in Settings and their tokens (or why the tokens file
/// couldn't be read: then none of them is used), less those turned off. One endpoint per
/// server: the CLI's current context first, then the first context naming it. When the
/// settings (`settings_ok`) couldn't be read, which servers are off is unknown: all are.
fn endpoints_from(
    cli: Option<Result<String, String>>,
    settings: &[String],
    tokens: Result<BTreeMap<String, String>, String>,
    off: &[String],
    settings_ok: bool,
    now: i64,
) -> Endpoints {
    let mut out = Endpoints::default();
    if !settings_ok {
        out.errors.push("settings.json can't be read: the Argo CD servers are off until it can".into());
    }
    let mut cli_tokens: HashMap<String, Option<String>> = HashMap::new();
    let mut entries = match cli {
        Some(Ok(text)) => cli_entries(&text, &mut out.notes).unwrap_or_else(|e| {
            out.errors.push(e);
            Vec::new()
        }),
        Some(Err(e)) => {
            out.errors.push(format!("the argocd CLI's config can't be read: {e}"));
            Vec::new()
        }
        None => Vec::new(),
    };
    entries.sort_by_key(|e| !e.current);
    for e in entries {
        let server = match e.server {
            Ok(s) => s,
            Err(why) => {
                out.errors.push(why);
                continue;
            }
        };
        if out.list.iter().any(|x| x.server.base == server.base) {
            continue;
        }
        cli_tokens.insert(server.base.clone(), e.token);
        out.list.push(Endpoint {
            server,
            cli_context: Some(e.context),
            in_settings: false,
            insecure: e.insecure,
            login: Err(String::new()),
            on: true,
            settings_token: false,
        });
    }
    for text in settings {
        match Server::parse(text) {
            Ok(server) => match out.list.iter_mut().find(|x| x.server.base == server.base) {
                Some(x) => x.in_settings = true,
                None => out.list.push(Endpoint {
                    server,
                    cli_context: None,
                    in_settings: true,
                    insecure: false,
                    login: Err(String::new()),
                    on: true,
                    settings_token: false,
                }),
            },
            Err(e) => out.errors.push(format!("Settings → Argo CD: {e}")),
        }
    }
    for x in &mut out.list {
        let name = x.name();
        let cli = cli_tokens.get(&x.server.base).cloned().flatten();
        x.login = match &tokens {
            Ok(tokens) => login_of(tokens.get(&x.server.base).map(String::as_str), cli.as_deref(), &name, now),
            // Not knowing which token Settings has, none is used: not even the CLI's.
            Err(e) => Err(format!("The Argo CD tokens file can't be read ({e}): set the token again")),
        };
        x.on = settings_ok && !off.contains(&x.server.base);
        x.settings_token = tokens.as_ref().is_ok_and(|t| t.get(&x.server.base).is_some_and(|t| !t.trim().is_empty()));
    }
    out
}

fn tokens_path() -> std::path::PathBuf {
    crate::paths::config_dir().join(TOKENS_FILE)
}

/// The tokens set in Settings → Argo CD, by server address; none when there is no file. A
/// file that can't be read or doesn't parse is an error: no token is guessed.
fn read_tokens() -> Result<BTreeMap<String, String>, String> {
    match std::fs::read(tokens_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", tokens_path().display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(format!("{}: {e}", tokens_path().display())),
    }
}

/// Keep `token` for the server at `base` (empty: forget it), readable only by the user. A
/// tokens file that doesn't read is kept aside first, as other config is.
pub fn save_token(base: &str, token: &str) -> std::io::Result<()> {
    let mut tokens = read_tokens().unwrap_or_else(|_| {
        crate::settings::keep_aside(&tokens_path());
        BTreeMap::new()
    });
    let token = token.trim();
    if token.is_empty() {
        tokens.remove(base);
    } else {
        tokens.insert(base.to_string(), token.to_string());
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

/// The servers to read: the argocd CLI's and those set in Settings. Reads files: call it off
/// the UI thread when it can wait.
pub fn endpoints() -> Endpoints {
    let path = server::cli_config_path();
    let cli = match std::fs::read_to_string(&path) {
        Ok(text) => Some(Ok(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => Some(Err(format!("{}: {e}", path.display()))),
    };
    let (settings, off) = crate::settings::with(|s| (s.argocd_api_servers.clone(), s.argocd_api_off.clone()));
    let settings_ok = crate::settings::load_error().is_none();
    endpoints_from(cli, &settings, read_tokens(), &off, settings_ok, jiff::Timestamp::now().as_second())
}

/// When the argocd CLI's config and the tokens file last changed (and their sizes): a page
/// compares it each second, to notice a new `argocd login` without being told.
pub fn config_stamp() -> Vec<Option<(std::time::SystemTime, u64)>> {
    [server::cli_config_path(), tokens_path()]
        .iter()
        .map(|p| std::fs::metadata(p).ok().and_then(|m| Some((m.modified().ok()?, m.len()))))
        .collect()
}

// MARK: HTTP

/// What a request needs: where, the token, and whether the certificate is checked.
#[derive(Clone, Debug)]
struct Conn {
    server: Server,
    token: Token,
    insecure: bool,
}

/// A request that failed: the HTTP status when the server answered, and what to show
/// (never the token).
#[derive(Debug)]
pub(super) struct Failure {
    pub status: Option<u16>,
    pub text: String,
}

type HttpClient = hyper_util::client::legacy::Client<
    hyper_rustls::HttpsConnector<hyper_util::client::legacy::connect::HttpConnector>,
    Full<Bytes>,
>;

/// Accepts any certificate, still checking the handshake's signatures: the CLI's
/// `--insecure`, for a server with a self-signed certificate.
#[derive(Debug)]
struct AnyCertificate(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AnyCertificate {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

/// The client for servers whose certificate is checked, or not (`insecure`): made once, its
/// connections kept for the next requests (each server's own).
fn http_client(insecure: bool) -> anyhow::Result<HttpClient> {
    static CLIENTS: LazyLock<Mutex<HashMap<bool, HttpClient>>> = LazyLock::new(Default::default);
    let mut clients = CLIENTS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(c) = clients.get(&insecure) {
        return Ok(c.clone());
    }
    let client = build_client(insecure)?;
    clients.insert(insecure, client.clone());
    Ok(client)
}

fn build_client(insecure: bool) -> anyhow::Result<HttpClient> {
    let mut http = hyper_util::client::legacy::connect::HttpConnector::new();
    http.enforce_http(false);
    http.set_connect_timeout(Some(CONNECT_TIMEOUT));
    // A stream's connection that dies quietly is noticed.
    http.set_keepalive(Some(Duration::from_secs(60)));
    let builder = hyper_rustls::HttpsConnectorBuilder::new();
    let builder = if insecure {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let config = rustls::ClientConfig::builder_with_provider(provider.clone())
            .with_safe_default_protocol_versions()?
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AnyCertificate(provider)))
            .with_no_client_auth();
        builder.with_tls_config(config)
    } else {
        builder.with_native_roots()?
    };
    let https = builder.https_or_http().enable_http1().wrap_connector(http);
    Ok(hyper_util::client::legacy::Client::builder(hyper_util::rt::TokioExecutor::new()).build(https))
}

/// A request to `url` with the token; its JSON answer (`null` when empty). At most
/// `server::MAX_BYTES` read, within `server::TIMEOUT`; redirects are not followed, so the
/// token never goes to another address. What fails never holds the token.
pub(super) async fn send(
    method: http::Method,
    url: String,
    token: &str,
    body: Option<&Value>,
    insecure: bool,
    server: &Server,
) -> Result<Value, Failure> {
    send_within(method, url, token, body, insecure, server, server::TIMEOUT).await
}

/// The same, waiting up to `timeout` for the whole answer.
pub(super) async fn send_within(
    method: http::Method,
    url: String,
    token: &str,
    body: Option<&Value>,
    insecure: bool,
    server: &Server,
    timeout: Duration,
) -> Result<Value, Failure> {
    let fail = |status: Option<u16>, text: String| Failure { status, text: scrub(&text, token) };
    let fetch = async {
        let client = http_client(insecure).map_err(|e| fail(None, format!("{e:#}")))?;
        let mut req = http::Request::builder()
            .method(method)
            .uri(&url)
            .header("user-agent", "recouvrance")
            .header("accept", "application/json")
            .header("authorization", format!("Bearer {token}"));
        if body.is_some() {
            req = req.header("content-type", "application/json");
        }
        let bytes = body.map_or_else(Bytes::new, |b| Bytes::from(b.to_string()));
        let req = req.body(Full::new(bytes)).map_err(|e| fail(None, e.to_string()))?;
        let resp = client.request(req).await.map_err(|e| {
            fail(None, format!("can't reach Argo CD at {}: {:#}", server.name(), anyhow::Error::from(e)))
        })?;
        let (parts, body) = resp.into_parts();
        let bytes = http_body_util::Limited::new(body, server::MAX_BYTES).collect().await.map_err(|e| {
            if e.is::<http_body_util::LengthLimitError>() {
                fail(None, format!("Argo CD's answer is larger than {} MB", server::MAX_BYTES / (1024 * 1024)))
            } else {
                fail(None, format!("Argo CD's answer was cut: {e}"))
            }
        })?;
        let bytes = bytes.to_bytes();
        if !parts.status.is_success() {
            let said = server::refusal(parts.status, &String::from_utf8_lossy(&bytes), server);
            return Err(fail(Some(parts.status.as_u16()), said));
        }
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Value::Null);
        }
        serde_json::from_slice(&bytes)
            .map_err(|_| fail(None, "Argo CD's answer isn't JSON: is this its API server's address?".into()))
    };
    match tokio::time::timeout(timeout, fetch).await {
        Ok(result) => result,
        Err(_) => {
            Err(fail(None, format!("Argo CD at {} did not answer within {} s", server.name(), timeout.as_secs())))
        }
    }
}

impl Conn {
    async fn call(&self, method: http::Method, path: &str, body: Option<&Value>) -> Result<Value, Failure> {
        let url = format!("{}{path}", self.server.base);
        send(method, url, &self.token.0, body, self.insecure, &self.server).await
    }

    async fn get(&self, path: &str) -> Result<Value, Failure> {
        self.call(http::Method::GET, path, None).await
    }

    /// A GET that may take up to `timeout` (Argo CD rendering manifests).
    async fn get_within(&self, path: &str, timeout: Duration) -> Result<Value, Failure> {
        let url = format!("{}{path}", self.server.base);
        send_within(http::Method::GET, url, &self.token.0, None, self.insecure, &self.server, timeout).await
    }

    /// Open the application stream: its body, read as it comes. Argo CD subscribes it to
    /// every change, then sends each Application as ADDED, then the changes: nothing is
    /// missed from the moment it is open. Its answer may wait for the first event.
    async fn stream(&self) -> Result<hyper::body::Incoming, Failure> {
        let fail = |status: Option<u16>, text: String| Failure { status, text: scrub(&text, &self.token.0) };
        let client = http_client(self.insecure).map_err(|e| fail(None, format!("{e:#}")))?;
        let url = format!("{}{}", self.server.base, stream_path());
        let req = http::Request::get(&url)
            .header("user-agent", "recouvrance")
            .header("accept", "text/event-stream")
            .header("authorization", format!("Bearer {}", self.token.0))
            .body(Full::new(Bytes::new()))
            .map_err(|e| fail(None, e.to_string()))?;
        let resp = tokio::time::timeout(IDLE, client.request(req))
            .await
            .map_err(|_| fail(None, format!("Argo CD at {} did not open its stream", self.server.name())))?
            .map_err(|e| {
                fail(None, format!("can't reach Argo CD at {}: {:#}", self.server.name(), anyhow::Error::from(e)))
            })?;
        let (parts, body) = resp.into_parts();
        if !parts.status.is_success() {
            let read = http_body_util::Limited::new(body, 64 * 1024).collect();
            let text = match tokio::time::timeout(Duration::from_secs(5), read).await {
                Ok(Ok(b)) => String::from_utf8_lossy(&b.to_bytes()).into_owned(),
                _ => String::new(),
            };
            return Err(fail(Some(parts.status.as_u16()), server::refusal(parts.status, &text, &self.server)));
        }
        Ok(body)
    }
}

/// A field asked of the stream only: each resource's result of the last operation, for the
/// sync journal. The list, which sends every Application at once, leaves it out.
const STREAM_ONLY: &str = "status.operationState.syncResult.resources";

/// `fields`, each with `prefix` (`items.` for a list, `result.application.` for the stream);
/// `STREAM_ONLY` only with `stream`.
fn fields(prefix: &str, stream: bool) -> String {
    APP_FIELDS
        .iter()
        .filter(|f| stream || **f != STREAM_ONLY)
        .map(|f| encode(&format!("{prefix}{f}")))
        .collect::<Vec<_>>()
        .join(",")
}

fn list_path() -> String {
    format!("/api/v1/applications?fields=metadata.resourceVersion,{}", fields("items.", false))
}

fn stream_path() -> String {
    format!("/api/v1/stream/applications?fields=result.type,{}", fields("result.application.", true))
}

fn app_path(namespace: &str, name: &str) -> String {
    format!("/api/v1/applications/{}?appNamespace={}", encode(name), encode(namespace))
}

// MARK: Applications

/// Set `path` in `out` to `v`, making the objects on the way.
fn put(out: &mut Value, path: &[&str], v: Value) {
    let Some((last, parents)) = path.split_last() else { return };
    let mut at = out;
    for p in parents {
        if !at.get(*p).is_some_and(Value::is_object) {
            at[*p] = Value::Object(Map::new());
        }
        at = &mut at[*p];
    }
    at[*last] = v;
}

/// What is kept of an Application: what the page reads, whatever the server sent (it may
/// ignore `fields`). Memory stays small however many there are: no resources, no
/// manifests, the history's last deployments without their sources.
pub fn lean(app: &Value) -> Value {
    const KEPT: [&str; 19] = [
        "/metadata/name",
        "/metadata/namespace",
        "/metadata/creationTimestamp",
        "/metadata/labels",
        "/metadata/deletionTimestamp",
        "/metadata/resourceVersion",
        "/spec/project",
        "/spec/destination",
        "/spec/syncPolicy",
        "/status/sync/status",
        "/status/sync/revision",
        "/status/sync/revisions",
        "/status/health/status",
        "/status/health/message",
        "/status/operationState/phase",
        "/status/operationState/message",
        "/status/operationState/startedAt",
        "/status/operationState/finishedAt",
        "/status/resourceHealthSource",
    ];
    let mut out = json!({});
    for p in KEPT {
        if let Some(v) = app.pointer(p).filter(|v| !v.is_null()) {
            put(&mut out, &p[1..].split('/').collect::<Vec<_>>(), v.clone());
        }
    }
    // The tracking id alone: an app of apps is found by it (not kubectl's copy of the object).
    if let Some(id) = app.pointer("/metadata/annotations").and_then(|a| a.get(TRACKING_ID)) {
        put(&mut out, &["metadata", "annotations", TRACKING_ID], id.clone());
    }
    // An operation running or waiting: whether, not what.
    if app.get("operation").is_some_and(|o| !o.is_null()) {
        out["operation"] = json!({});
    }
    let all = app.pointer("/status/history").and_then(Value::as_array).map_or(&[][..], Vec::as_slice);
    let history: Vec<Value> = all[all.len().saturating_sub(HISTORY)..]
        .iter()
        .map(|h| {
            let mut kept = Map::new();
            for k in ["id", "revision", "revisions", "deployedAt", "initiatedBy"] {
                if let Some(v) = h.get(k) {
                    kept.insert(k.into(), v.clone());
                }
            }
            Value::Object(kept)
        })
        .collect();
    if !history.is_empty() {
        put(&mut out, &["status", "history"], Value::Array(history));
    }
    let conditions: Vec<Value> = app
        .pointer("/status/conditions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| json!({"type": c["type"], "message": c["message"]}))
        .collect();
    if !conditions.is_empty() {
        put(&mut out, &["status", "conditions"], Value::Array(conditions));
    }
    // Set by annotations in the manifests: only web links are kept.
    if let Some(urls) = app.pointer("/status/summary/externalURLs").and_then(Value::as_array) {
        let web: Vec<Value> =
            urls.iter().filter(|u| u.as_str().is_some_and(crate::forwards::is_web_link)).cloned().collect();
        put(&mut out, &["status", "summary", "externalURLs"], Value::Array(web));
    }
    out
}

/// An Application read by the API: what is kept of it, and its summary.
#[derive(Clone, Debug)]
pub struct ApiApp {
    pub value: Value,
    pub summary: Summary,
}

impl ApiApp {
    fn of(app: &Value) -> Self {
        let value = lean(app);
        let mut summary = Summary::of(&value);
        if summary.namespace.is_empty() {
            summary.namespace = DEFAULT_NAMESPACE.into();
        }
        Self { value, summary }
    }

    fn key(&self) -> (String, String) {
        (self.summary.namespace.clone(), self.summary.name.clone())
    }

    fn resource_version(&self) -> Option<u64> {
        self.value.pointer("/metadata/resourceVersion").and_then(Value::as_str)?.parse().ok()
    }
}

/// One change the stream says.
#[derive(Clone, Debug)]
pub(super) enum Event {
    /// Added or changed: the Application as it is now.
    Upsert(Box<ApiApp>),
    Deleted {
        namespace: String,
        name: String,
    },
}

/// Server-sent events read from a stream as bytes come: the data of each event complete. A
/// line is only read once whole, so a character split across two reads stays whole. A line
/// that is a JSON object alone is an event too (the stream without `text/event-stream`).
#[derive(Default)]
pub(super) struct Sse {
    buf: Vec<u8>,
    /// The start of `buf` known to hold no line break: a long line coming in many reads is
    /// scanned once, not again on each.
    scanned: usize,
    data: String,
    has_data: bool,
}

impl Sse {
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<String>, String> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        let mut start = 0;
        let mut from = self.scanned;
        while let Some(end) = self.buf[from..].iter().position(|b| *b == b'\n').map(|i| from + i) {
            let line = String::from_utf8_lossy(&self.buf[start..end]).into_owned();
            start = end + 1;
            from = start;
            let line = line.strip_suffix('\r').unwrap_or(&line);
            if line.is_empty() {
                if std::mem::take(&mut self.has_data) {
                    out.push(std::mem::take(&mut self.data));
                }
                continue;
            }
            if line.starts_with('{') && !self.has_data {
                out.push(line.to_string());
                continue;
            }
            if line.starts_with(':') {
                continue;
            }
            let (field, value) =
                line.split_once(':').map_or((line, ""), |(f, v)| (f, v.strip_prefix(' ').unwrap_or(v)));
            if field == "data" {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
        }
        self.buf.drain(..start);
        self.scanned = self.buf.len();
        if self.buf.len() + self.data.len() > MAX_EVENT {
            return Err(format!("an event of the stream is larger than {} MB", MAX_EVENT / (1024 * 1024)));
        }
        Ok(out)
    }
}

/// One event of the stream, as `parse_event_seen` reads it, the Application shown to nothing.
#[cfg(test)]
pub(super) fn parse_event(data: &str) -> Result<Option<Event>, String> {
    parse_event_seen(data, |_| {})
}

/// One event of the stream: `{"result": {"type": "ADDED", "application": {...}}}`. None for
/// one this doesn't use; an error the stream sends is an error. Each Application added or
/// changed is shown to `seen` whole, as sent (the sync journal keeps its operation).
fn parse_event_seen(data: &str, seen: impl FnOnce(&Value)) -> Result<Option<Event>, String> {
    let v: Value = serde_json::from_str(data).map_err(|e| format!("an event of the stream isn't JSON: {e}"))?;
    if let Some(e) = v.get("error").filter(|e| !e.is_null()) {
        let said = e.get("message").and_then(Value::as_str).unwrap_or("an error");
        return Err(format!("the stream says: {said}"));
    }
    let Some(result) = v.get("result") else { return Ok(None) };
    let Some(app) = result.get("application").filter(|a| a.is_object()) else { return Ok(None) };
    let name = app.pointer("/metadata/name").and_then(Value::as_str).unwrap_or_default();
    if name.is_empty() {
        return Ok(None);
    }
    Ok(match result.get("type").and_then(Value::as_str) {
        Some("ADDED" | "MODIFIED") => {
            seen(app);
            Some(Event::Upsert(Box::new(ApiApp::of(app))))
        }
        Some("DELETED") => {
            let ns = app.pointer("/metadata/namespace").and_then(Value::as_str).filter(|s| !s.is_empty());
            Some(Event::Deleted { namespace: ns.unwrap_or(DEFAULT_NAMESPACE).to_string(), name: name.to_string() })
        }
        _ => None,
    })
}

/// The Applications of a server, by namespace and name.
pub type Apps = HashMap<(String, String), Arc<ApiApp>>;

/// Apply an event: an Application added or changed replaces what was kept of it whole (a
/// field it no longer has, such as a finished operation, is gone), unless it is older than
/// what is kept; one deleted is gone.
pub(super) fn apply(apps: &mut Apps, event: Event) {
    match event {
        Event::Upsert(app) => {
            let key = app.key();
            let older = match (apps.get(&key).and_then(|a| a.resource_version()), app.resource_version()) {
                (Some(kept), Some(new)) => new < kept,
                _ => false,
            };
            if !older {
                apps.insert(key, Arc::new(*app));
            }
        }
        Event::Deleted { namespace, name } => {
            apps.remove(&(namespace, name));
        }
    }
}

// MARK: Feeds

/// What is known of one server's Applications.
#[derive(Clone, Debug, Default)]
pub struct FeedState {
    pub apps: Apps,
    /// Each project's sync windows, once read; or why they couldn't be.
    pub projects: Option<Result<super::ProjectWindows, String>>,
    /// Why the last read or stream failed (never the token).
    pub error: Option<String>,
    /// Listed at least once.
    pub listed: bool,
    /// The stream is open.
    pub live: bool,
}

struct Feed {
    state: Arc<Mutex<FeedState>>,
    users: usize,
    _task: crate::k8s::TaskGuard,
}

static FEEDS: LazyLock<Mutex<HashMap<String, Feed>>> = LazyLock::new(Default::default);
/// Counts every change of any feed: a page reads the feeds again when it moved.
static CHANGES: AtomicU64 = AtomicU64::new(0);
/// Counts the changes of the servers' config: feeds of an older one are left.
static GENERATION: AtomicU64 = AtomicU64::new(0);

/// Changes of any feed so far.
pub fn changes() -> u64 {
    CHANGES.load(Ordering::SeqCst)
}

/// The servers' config as last changed in Settings.
pub fn generation() -> u64 {
    GENERATION.load(Ordering::SeqCst)
}

/// The servers or their tokens changed: pages watch them again, with the new config.
pub fn reload() {
    GENERATION.fetch_add(1, Ordering::SeqCst);
    CHANGES.fetch_add(1, Ordering::SeqCst);
}

fn changed() {
    CHANGES.fetch_add(1, Ordering::SeqCst);
}

/// A page's hold on a server's feed: it runs while one is held, shared by every page.
pub struct Watch {
    pub endpoint: Arc<Endpoint>,
    pub state: Arc<Mutex<FeedState>>,
    key: String,
}

impl Drop for Watch {
    fn drop(&mut self) {
        let mut feeds = FEEDS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(f) = feeds.get_mut(&self.key) {
            f.users = f.users.saturating_sub(1);
            if f.users == 0 {
                // Dropping it stops its task: nothing more is read until a page shows again.
                feeds.remove(&self.key);
            }
        }
    }
}

/// Hold the feed of `endpoint`, starting it if none runs. Feeds are kept by server, login
/// and config generation: a new login or a change in Settings starts a new one.
pub fn watch(endpoint: &Endpoint) -> Watch {
    let key = watch_key(endpoint);
    let mut feeds = FEEDS.lock().unwrap_or_else(PoisonError::into_inner);
    let feed = feeds.entry(key.clone()).or_insert_with(|| {
        let state = Arc::new(Mutex::new(FeedState::default()));
        let task = crate::k8s::spawn_guarded(feed(endpoint.clone(), state.clone()));
        Feed { state, users: 0, _task: task }
    });
    feed.users += 1;
    Watch { endpoint: Arc::new(endpoint.clone()), state: feed.state.clone(), key }
}

/// What a feed of `endpoint` is kept under: its server, login and the config's generation.
pub(crate) fn watch_key(endpoint: &Endpoint) -> String {
    format!("{}#{}", endpoint.feed_key(), generation())
}

/// The feed of `endpoint` when a page holds one now, without holding it: a reader that
/// shouldn't open a stream of its own (a Problems scan) reads it while it runs.
pub fn running(endpoint: &Endpoint) -> Option<Arc<Mutex<FeedState>>> {
    FEEDS.lock().unwrap_or_else(PoisonError::into_inner).get(&watch_key(endpoint)).map(|f| f.state.clone())
}

fn lock(state: &Mutex<FeedState>) -> std::sync::MutexGuard<'_, FeedState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

/// How a round of a feed ended.
enum Ended {
    /// The stream ended or went silent, as streams do: opened again, nothing to show.
    Routine,
    /// The list or the stream failed: said on the page.
    Failed(Failure),
}

/// A server's feed: open the stream, list, follow; again after a backoff when it ends. A
/// refused login stops it until the login or the config changes.
async fn feed(endpoint: Endpoint, state: Arc<Mutex<FeedState>>) {
    let conn = match endpoint.conn() {
        Ok(c) => c,
        Err(why) => {
            lock(&state).error = Some(why);
            changed();
            return;
        }
    };
    let mut backoff = BACKOFF;
    loop {
        let started = Instant::now();
        let ended = follow(&conn, &state).await;
        let refused = {
            let mut s = lock(&state);
            s.live = false;
            match ended {
                Ended::Routine => {
                    s.error = None;
                    false
                }
                Ended::Failed(f) => {
                    s.error = Some(f.text);
                    f.status == Some(401)
                }
            }
        };
        changed();
        if refused {
            return;
        }
        if started.elapsed() >= STEADY {
            backoff = BACKOFF;
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(MAX_BACKOFF);
    }
}

/// The list, with the changes the stream sent while it was read applied after it, in their
/// order: those older than what the list has are left (`apply`), a deletion removes.
pub(super) fn settle(listed: Apps, sent: Vec<Event>) -> Apps {
    let mut apps = listed;
    for event in sent {
        apply(&mut apps, event);
    }
    apps
}

/// What the stream's reader passes on: events, or how the stream ended.
type Batch = Result<Vec<Event>, Ended>;

/// Read the stream and pass its events on, in order, until it ends.
async fn read_stream(conn: Conn, state: Arc<Mutex<FeedState>>, out: tokio::sync::mpsc::Sender<Batch>) {
    let mut body = match conn.stream().await {
        Ok(body) => body,
        Err(f) => {
            let _ = out.send(Err(Ended::Failed(f))).await;
            return;
        }
    };
    lock(&state).live = true;
    changed();
    let instance = super::grouping::server_instance(&conn.server.base);
    let mut sse = Sse::default();
    let failed = |text: String| Ended::Failed(Failure { status: None, text: scrub(&text, &conn.token.0) });
    let ended = loop {
        let frame = match tokio::time::timeout(IDLE, body.frame()).await {
            // Silent for 15 min, ended, or broken: opened again, as streams are.
            Err(_) | Ok(None | Some(Err(_))) => break Ended::Routine,
            Ok(Some(Ok(frame))) => frame,
        };
        let Some(data) = frame.data_ref() else { continue };
        let events = match sse.push(data) {
            Ok(events) => events,
            Err(e) => break failed(e),
        };
        let mut batch = Vec::with_capacity(events.len());
        let mut bad = None;
        for data in events {
            match parse_event_seen(&data, |app| super::journal::observe(&instance, app)) {
                Ok(Some(event)) => batch.push(event),
                Ok(None) => {}
                Err(e) => {
                    bad = Some(e);
                    break;
                }
            }
        }
        if !batch.is_empty() && out.send(Ok(batch)).await.is_err() {
            return;
        }
        if let Some(e) = bad {
            break failed(e);
        }
    };
    let _ = out.send(Err(ended)).await;
}

/// One round: open the stream first, so every change from then on is in it; list; apply what
/// the stream sent meanwhile over the list; then follow the stream. The sync windows are read
/// beside it, again every `PROJECTS_EVERY`. What ended it.
async fn follow(conn: &Conn, state: &Arc<Mutex<FeedState>>) -> Ended {
    // Changes pile up here while the list is read (bounded: the stream waits when it's full).
    let (tx, mut rx) = tokio::sync::mpsc::channel::<Batch>(1024);
    let _stream = crate::k8s::spawn_guarded(read_stream(conn.clone(), state.clone(), tx));
    let list = match conn.get(&list_path()).await {
        Ok(v) => v,
        Err(f) => return Ended::Failed(f),
    };
    // Not kept in the sync journal from here: the list leaves out each resource's result, and
    // the stream, open first, sends every Application whole with it.
    let listed: Apps = listed(&list).map(|a| (a.key(), Arc::new(a))).collect();
    drop(list);
    let mut sent = Vec::new();
    while let Ok(batch) = rx.try_recv() {
        match batch {
            Ok(events) => sent.extend(events),
            Err(ended) => return ended,
        }
    }
    {
        let mut s = lock(state);
        s.apps = settle(listed, sent);
        s.listed = true;
        s.error = None;
    }
    changed();
    let _windows = crate::k8s::spawn_guarded(keep_windows(conn.clone(), state.clone()));
    loop {
        match rx.recv().await {
            Some(Ok(events)) => {
                let mut s = lock(state);
                for event in events {
                    apply(&mut s.apps, event);
                }
                drop(s);
                changed();
            }
            Some(Err(ended)) => return ended,
            None => return Ended::Routine,
        }
    }
}

/// The Applications of a list's answer, as kept.
fn listed(list: &Value) -> impl Iterator<Item = ApiApp> + '_ {
    list.get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(ApiApp::of)
        .filter(|a| !a.summary.name.is_empty())
}

/// Read the projects' sync windows now and every `PROJECTS_EVERY` while the feed follows.
async fn keep_windows(conn: Conn, state: Arc<Mutex<FeedState>>) {
    loop {
        let projects: Vec<String> = {
            let s = lock(&state);
            let mut p: Vec<String> = s.apps.values().map(|a| a.summary.project.clone()).collect();
            p.sort();
            p.dedup();
            p
        };
        let windows = read_windows(&conn, projects).await;
        lock(&state).projects = Some(windows.map(Arc::new));
        changed();
        tokio::time::sleep(PROJECTS_EVERY).await;
    }
}

/// Each project's sync windows: one list of the projects, else (when this login may not
/// list them) each project of the Applications read on its own (`spec.syncWindows`).
async fn read_windows(conn: &Conn, projects: Vec<String>) -> Result<BTreeMap<String, Vec<SyncWindow>>, String> {
    let listing = match conn.get("/api/v1/projects").await {
        Ok(listed) => {
            return Ok(listed
                .get("items")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|p| Some((p.pointer("/metadata/name")?.as_str()?.to_string(), windows::windows_of(p))))
                .collect());
        }
        Err(f) => f,
    };
    if projects.len() > MAX_PROJECTS {
        return Err(format!("its projects can't be listed ({}), and there are more than {MAX_PROJECTS}", listing.text));
    }
    let mut out = BTreeMap::new();
    for p in projects {
        match conn.get(&format!("/api/v1/projects/{}", encode(&p))).await {
            Ok(v) => {
                out.insert(p, windows::windows_of(&v));
            }
            Err(f) => return Err(format!("its projects' sync windows can't be read: {} ({})", f.text, listing.text)),
        }
    }
    Ok(out)
}

// MARK: One-off requests

/// Check a server: who the token logs in as, Argo CD's version, and how many Applications
/// it may read. What fails never holds the token.
pub async fn test(endpoint: Endpoint) -> Result<String, String> {
    let read = crate::k8s::run_abortable(async move {
        let conn = match endpoint.conn() {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        Ok(check(&conn).await.map_err(|f| f.text))
    });
    read.await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

async fn check(conn: &Conn) -> Result<String, Failure> {
    let me = conn.get("/api/v1/session/userinfo").await?;
    if me.get("loggedIn").and_then(Value::as_bool) != Some(true) {
        let name = conn.server.name();
        return Err(Failure {
            status: Some(401),
            text: format!("Argo CD at {name} doesn't take this token as a login: expired or revoked?"),
        });
    }
    let who = me.get("username").and_then(Value::as_str).filter(|u| !u.is_empty()).unwrap_or("a token");
    let version = conn.get("/api/version").await.ok();
    let version =
        version.as_ref().and_then(|v| v.get("Version")).and_then(Value::as_str).unwrap_or("of unknown version");
    let apps = conn.get("/api/v1/applications?fields=items.metadata.name").await?;
    let n = apps.get("items").and_then(Value::as_array).map_or(0, Vec::len);
    Ok(format!("Logged in as {who}, Argo CD {version}: {} readable", crate::plural::count(n, "Application")))
}

/// Every Application of `endpoint` its login may read: one list, no stream, each kept as
/// the feeds keep them, within `timeout`. For a reader with no feed running (`running`).
pub async fn list_once(endpoint: Endpoint, timeout: Duration) -> Result<Vec<Arc<ApiApp>>, String> {
    let read = crate::k8s::run_abortable(async move {
        let conn = match endpoint.conn() {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        let list = conn.get_within(&list_path(), timeout).await;
        Ok(list.map(|list| listed(&list).map(Arc::new).collect()).map_err(|f| f.text))
    });
    read.await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

/// The server at `base` as the config says now: a write goes with the login set now, and not
/// at all to a server removed or turned off since the page was drawn.
fn current(base: &str) -> Result<Endpoint, String> {
    let found = endpoints().list.into_iter().find(|e| e.server.base == base);
    match found {
        Some(e) if e.on => Ok(e),
        Some(_) => Err(format!("{base} is turned off in Settings → Argo CD")),
        None => Err(format!("{base} is no longer an Argo CD server set or logged in to")),
    }
}

/// The Application `namespace/name` as Argo CD has it now, whole (for a confirmation).
pub async fn read_app(endpoint: Arc<Endpoint>, namespace: String, name: String) -> Result<Value, String> {
    let read = crate::k8s::run_abortable(async move {
        let conn = match current(&endpoint.server.base).and_then(|e| e.conn()) {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        let app = conn.get(&app_path(&namespace, &name)).await.map_err(|f| f.text);
        if let Ok(app) = &app {
            super::journal::observe(&super::grouping::server_instance(&conn.server.base), app);
        }
        Ok(app)
    });
    read.await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

/// The dashboard of the Application `namespace/name`, read by the API with the server's login
/// as the config has it now: the Application whole, then its resource tree and managed
/// resources together, then the commits it deploys (each kept once read). Read-only.
pub async fn dashboard(
    endpoint: Arc<Endpoint>,
    namespace: String,
    name: String,
) -> Result<super::dashboard::Read, String> {
    let read = crate::k8s::run_abortable(async move {
        let found = current(&endpoint.server.base).and_then(|e| Ok((e.conn()?, e.login?.1)));
        let (conn, how) = match found {
            Ok(found) => found,
            Err(e) => return Ok(Err(e)),
        };
        let app = match conn.get(&app_path(&namespace, &name)).await {
            Ok(app) => app,
            Err(f) => return Ok(Err(f.text)),
        };
        super::journal::observe(&super::grouping::server_instance(&conn.server.base), &app);
        let shown = conn.server.name();
        let get = |path: String| {
            let conn = conn.clone();
            async move { conn.get(&path).await.map_err(|f| f.text) }
        };
        let diff_of = |v: &Value| server::diffs(v, shown.clone(), how);
        let base = conn.server.base.clone();
        Ok(Ok(super::dashboard::extras(&base, &namespace, &name, app, get, diff_of).await))
    });
    read.await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

/// The smart diff of the manifests Argo CD renders for the Application `namespace/name` at
/// two revisions, read by the API with the server's login as the config has it now (each
/// revision's kept once read). Read-only.
pub async fn manifests_diff(
    endpoint: Arc<Endpoint>,
    namespace: String,
    name: String,
    spec: u64,
    from: super::sync_diff::Revision,
    to: super::sync_diff::Revision,
) -> Result<Arc<crate::smartdiff::SmartDiff>, String> {
    use super::sync_diff::{RENDER_TIMEOUT, Target};
    let read = crate::k8s::run_abortable(async move {
        let conn = match current(&endpoint.server.base).and_then(|e| e.conn()) {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        let get = |path: String| {
            let conn = conn.clone();
            async move { conn.get_within(&path, RENDER_TIMEOUT).await.map_err(|f| f.text) }
        };
        let target = Target { server: conn.server.base.clone(), namespace, name, spec };
        Ok(super::sync_diff::manifests_diff(target, from, to, get).await)
    });
    read.await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

/// What a sync of the Application `namespace/name` would change now (its managed resources,
/// as a smart diff), read by the API with the server's login as the config has it now. The
/// diff is worked out in the same task, off the UI thread. Read-only.
pub async fn sync_preview(
    endpoint: Arc<Endpoint>,
    namespace: String,
    name: String,
) -> Result<Arc<super::grouped_diff::AppDiff>, String> {
    let read = crate::k8s::run_abortable(async move {
        let conn = match current(&endpoint.server.base).and_then(|e| e.conn()) {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        Ok(match conn.get(&super::dashboard::managed_path(&namespace, &name)).await {
            Ok(body) => super::grouped_diff::of_managed(body).await,
            Err(f) => Err(f.text),
        })
    });
    read.await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

/// What can be asked of an Application by the API.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Refresh {
        hard: bool,
    },
    /// A sync with the options asked for (the whole Application, hooks included, with none).
    Sync(super::ops::SyncRequest),
    Terminate,
    /// Back to the deployment of its history with this id.
    Rollback {
        id: i64,
    },
}

/// The request argocd-server's routes take for `action` on the Application `namespace/name`:
/// method, path and query, body.
pub(super) fn request(action: &Action, namespace: &str, name: &str) -> (http::Method, String, Option<Value>) {
    let app = format!("/api/v1/applications/{}", encode(name));
    let ns = encode(namespace);
    let body = json!({"name": name, "appNamespace": namespace});
    match action {
        Action::Refresh { hard } => {
            let mode = if *hard { "hard" } else { "normal" };
            (http::Method::GET, format!("{app}?appNamespace={ns}&refresh={mode}"), None)
        }
        Action::Sync(req) => (http::Method::POST, format!("{app}/sync"), Some(sync_body(body, req))),
        Action::Terminate => (http::Method::DELETE, format!("{app}/operation?appNamespace={ns}"), None),
        Action::Rollback { id } => {
            let mut body = body;
            body["id"] = json!(id);
            (http::Method::POST, format!("{app}/rollback"), Some(body))
        }
    }
}

/// The body of a sync request (argocd-server's `ApplicationSyncRequest`): the Application, its
/// strategy (hook, or apply: without hooks), and prune, dry run and resources when asked.
fn sync_body(mut body: Value, req: &super::ops::SyncRequest) -> Value {
    body["strategy"] = super::ops::strategy(req);
    // Always said, false too: Argo CD fills what isn't from the Application's own defaults
    // (`syncPolicy.manualDefaults`), which could prune or dry-run what wasn't asked.
    body["prune"] = json!(req.prune);
    body["dryRun"] = json!(req.dry_run);
    if let Some(rev) = &req.revision {
        body["revision"] = json!(rev);
    }
    if !req.resources.is_empty() {
        body["resources"] = Value::Array(req.resources.iter().map(super::ops::resource_entry).collect());
    }
    body
}

/// Ask Argo CD for `action` on the Application `namespace/name`, by its API, with the
/// server's login as the config has it now (`current`). Refused while write mode is off, or
/// the server (or a hub context tied to it) is set read-only, checked as it is sent, as the
/// UI's other writes are. A sync reads the Application right
/// before it is sent and is refused as argocd-server would refuse it (an operation running),
/// or when prune would now delete more than was shown.
pub async fn act(endpoint: Arc<Endpoint>, namespace: String, name: String, action: Action) -> Result<String, String> {
    act_read(endpoint, namespace, name, action).await.map(|(said, _)| said)
}

/// [`act`], also giving when the Application's last operation started as read right before a
/// sync was sent (None for other actions, or when it has none): the operation the sync
/// starts is a later one.
pub async fn act_read(
    endpoint: Arc<Endpoint>,
    namespace: String,
    name: String,
    action: Action,
) -> Result<(String, Option<String>), String> {
    let write = crate::k8s::run_abortable(async move {
        let conn = match current(&endpoint.server.base).and_then(|e| e.conn()) {
            Ok(c) => c,
            Err(e) => return Ok(Err(e)),
        };
        let mut read = None;
        if let Action::Sync(req) = &action {
            let app = match conn.get(&app_path(&namespace, &name)).await {
                Ok(app) => app,
                Err(f) => return Ok(Err(format!("the Application could not be read: {}", f.text))),
            };
            let op = super::ops::Op::Sync(req.clone());
            if let Some(why) = super::ops::refusal(&app, &op) {
                return Ok(Err(format!("Argo CD would refuse it: {why}")));
            }
            read = Some((app, op));
        }
        // A hub context tied to this server (Settings → Per cluster) is the same Argo CD:
        // its read-only setting counts, though the write goes through the API. Else the
        // server's name, as its protection is keyed.
        let server_name = endpoint.name();
        let servers = crate::settings::with(|s| s.argocd_servers.clone());
        let read_only =
            server::read_only_context(&servers, &endpoint.server.base, &server_name, crate::settings::is_read_only);
        if let Some(why) = read_only
            .map_or_else(|| crate::k8s::gate::refusal(&server_name), |c| Some(crate::k8s::gate::read_only_refusal(&c)))
        {
            return Ok(Err(why));
        }
        let (method, path, body) = request(&action, &namespace, &name);
        let sent = conn.call(method, &path, body.as_ref()).await;
        let before = read.as_ref().and_then(|(app, _)| super::grouped_sync::started_at(app));
        Ok(sent
            .map(|_| match &action {
                Action::Refresh { hard: false } => format!("Argo CD refreshes {name}"),
                Action::Refresh { hard: true } => format!("Argo CD hard-refreshes {name}"),
                Action::Sync(_) => match &read {
                    Some((app, op)) => super::ops::started(app, op, &name),
                    None => format!("Sync of {name} started"),
                },
                Action::Terminate => format!("The operation of {name} is terminating"),
                Action::Rollback { id } => format!("Rollback of {name} to deployment {id} started"),
            })
            .map(|said| (said, before))
            .map_err(|f| f.text))
    });
    write.await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

#[cfg(test)]
mod tests {
    use super::{
        Action, Event, Sse, apply, endpoints_from, lean, list_path, parse_event, request, settle, stream_path,
    };
    use crate::k8s::argo::diagnosis::Res;
    use crate::k8s::argo::ops::SyncRequest;
    use crate::k8s::argo::server::Login;
    use base64::Engine as _;
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, HashMap};

    const CLI: &str = "
contexts:
- name: dev
  server: argocd-dev.example.com
  user: dev
- name: prod
  server: argocd.example.com
  user: prod
- name: prod-again
  server: argocd.example.com
  user: prod-again
- name: lab
  server: localhost:8080
  user: lab
- name: core
  server: kubernetes
  user: core
- name: remote-plain
  server: plain.example.com
  user: dev
- name: port-forward
  server: port-forward
  user: port-forward
current-context: prod-again
servers:
- server: argocd-dev.example.com
  grpc-web-root-path: /argo
  insecure: true
- server: argocd.example.com
  grpc-web: true
- server: localhost:8080
  plain-text: true
- server: kubernetes
  core: true
- server: plain.example.com
  plain-text: true
- server: port-forward
  plain-text: true
users:
- name: dev
  auth-token: dev-token-secret
- name: prod
  auth-token: prod-token-secret
- name: prod-again
  auth-token: prod-again-token-secret
- name: lab
  auth-token: ''
";

    fn jwt(exp: i64) -> String {
        let b = |v: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(v);
        format!("{}.{}.sig", b(b"{\"alg\":\"HS256\"}"), b(json!({"exp": exp}).to_string().as_bytes()))
    }

    #[test]
    fn the_cli_config_gives_one_server_per_address() {
        let e = endpoints_from(Some(Ok(CLI.into())), &[], Ok(BTreeMap::new()), &[], true, 100);
        let bases: Vec<&str> = e.list.iter().map(|x| x.server.base.as_str()).collect();
        // The current context first; core mode has no API server.
        assert_eq!(
            bases,
            ["https://argocd.example.com", "https://argocd-dev.example.com/argo", "http://localhost:8080"]
        );
        assert_eq!(e.list[0].cli_context.as_deref(), Some("prod-again"));
        assert!(matches!(&e.list[0].login, Ok((_, Login::Cli))));
        assert!(e.list[1].insecure && !e.list[0].insecure);
        // Logged out: no token, and how to log in.
        assert!(e.list[2].login.as_ref().is_err_and(|why| why.contains("argocd login localhost:8080")));
        // Plain http to another machine would send the token in clear: not used, said why.
        assert_eq!(e.errors.len(), 1, "{:?}", e.errors);
        assert!(e.errors[0].contains("remote-plain"));
        // `argocd login --port-forward` (#1075): no address to ask, said quietly, not an error.
        assert_eq!(e.notes.len(), 1, "{:?}", e.notes);
        assert!(e.notes[0].starts_with("argocd CLI context port-forward uses the CLI's port-forward mode"));
    }

    #[test]
    fn no_token_is_ever_printed() {
        let e = endpoints_from(Some(Ok(CLI.into())), &[], Ok(BTreeMap::new()), &[], true, 100);
        let shown = format!("{e:?}");
        for secret in ["dev-token-secret", "prod-token-secret", "prod-again-token-secret"] {
            assert!(!shown.contains(secret), "{shown}");
        }
        assert!(shown.contains("[token]"));
    }

    #[test]
    fn settings_add_servers_and_their_token_wins() {
        let mut tokens = BTreeMap::new();
        tokens.insert("https://argocd.example.com".to_string(), "from-settings".to_string());
        tokens.insert("https://extra.example.com".to_string(), jwt(50));
        let settings = ["argocd.example.com".to_string(), "https://extra.example.com/".into(), "ftp://x".into()];
        let off = ["http://localhost:8080".to_string()];
        let e = endpoints_from(Some(Ok(CLI.into())), &settings, Ok(tokens), &off, true, 100);
        let prod = &e.list[0];
        assert!(prod.in_settings && prod.cli_context.is_some());
        assert!(matches!(&prod.login, Ok((_, Login::Settings))));
        let extra = e.list.iter().find(|x| x.server.base == "https://extra.example.com").unwrap();
        assert!(extra.login.as_ref().is_err_and(|why| why.contains("expired")));
        assert!(!e.list.iter().find(|x| x.server.base == "http://localhost:8080").unwrap().on);
        assert!(e.errors.iter().any(|x| x.starts_with("Settings")));
        // A token set here, even expired, can be forgotten; the CLI's alone can't.
        assert!(prod.settings_token && extra.settings_token);
        assert!(!e.list.iter().find(|x| x.server.base.contains("argocd-dev")).unwrap().settings_token);
    }

    #[test]
    fn config_that_does_not_read_fails_closed() {
        // A CLI config that doesn't parse gives no server, and says so.
        let e = endpoints_from(Some(Ok("contexts: [".into())), &[], Ok(BTreeMap::new()), &[], true, 100);
        assert!(e.list.is_empty() && e.errors.len() == 1);
        let e = endpoints_from(Some(Err("permission denied".into())), &[], Ok(BTreeMap::new()), &[], true, 100);
        assert!(e.list.is_empty() && e.errors[0].contains("permission denied"));
        // A tokens file that doesn't read: no token is used, not even the CLI's.
        let e = endpoints_from(Some(Ok(CLI.into())), &[], Err("bad json".into()), &[], true, 100);
        assert!(e.list.iter().all(|x| x.login.is_err()));
        assert_eq!(endpoints_from(None, &[], Ok(BTreeMap::new()), &[], true, 100).list.len(), 0);
        // settings.json unreadable: which servers are off is unknown, so all are.
        let e = endpoints_from(Some(Ok(CLI.into())), &[], Ok(BTreeMap::new()), &[], false, 100);
        assert!(!e.list.is_empty() && e.list.iter().all(|x| !x.on));
        assert!(e.errors.iter().any(|x| x.contains("settings.json")));
    }

    #[test]
    fn each_action_is_the_request_argo_cd_routes() {
        let req = |a: Action| {
            let (m, path, body) = request(&a, "team a", "web");
            (m.to_string(), path, body)
        };
        let get = |path: &str| ("GET".to_string(), path.to_string(), None);
        assert_eq!(
            req(Action::Refresh { hard: false }),
            get("/api/v1/applications/web?appNamespace=team%20a&refresh=normal")
        );
        assert_eq!(
            req(Action::Refresh { hard: true }),
            get("/api/v1/applications/web?appNamespace=team%20a&refresh=hard")
        );
        let body =
            json!({"name": "web", "appNamespace": "team a", "strategy": {"hook": {}}, "prune": false, "dryRun": false});
        let sync = |r: SyncRequest| req(Action::Sync(r));
        assert_eq!(sync(SyncRequest::default()), ("POST".into(), "/api/v1/applications/web/sync".into(), Some(body)));
        assert_eq!(
            req(Action::Terminate),
            ("DELETE".into(), "/api/v1/applications/web/operation?appNamespace=team%20a".into(), None)
        );
        let body = json!({"name": "web", "appNamespace": "team a", "id": 7});
        assert_eq!(
            req(Action::Rollback { id: 7 }),
            ("POST".into(), "/api/v1/applications/web/rollback".into(), Some(body))
        );
    }

    /// Each option of a sync is in its request: the strategy (hook, or apply without hooks),
    /// prune, dry run and the resources picked; none is sent when not asked.
    #[test]
    fn a_sync_request_carries_each_option() {
        let body = |r: SyncRequest| request(&Action::Sync(r), "argocd", "web").2.unwrap();
        let plain = body(SyncRequest::default());
        // Prune and dry run are said even when off: Argo CD would fill them from the
        // Application's manual sync defaults otherwise.
        assert_eq!((plain["prune"].clone(), plain["dryRun"].clone()), (json!(false), json!(false)));
        assert!(plain.get("resources").is_none() && plain.get("revision").is_none());
        let pinned = body(SyncRequest { revision: Some("abc".into()), ..Default::default() });
        assert_eq!(pinned["revision"], "abc");
        assert_eq!(body(SyncRequest { apply: true, ..Default::default() })["strategy"], json!({"apply": {}}));
        let all = body(SyncRequest {
            resources: vec![
                Res {
                    group: "apps".into(),
                    kind: "Deployment".into(),
                    namespace: "shop".into(),
                    name: "web".into(),
                    ..Default::default()
                },
                Res { kind: "Namespace".into(), name: "shop".into(), ..Default::default() },
            ],
            prune: true,
            dry_run: true,
            apply: false,
            revision: None,
            prune_shown: None,
        });
        assert_eq!(all["strategy"], json!({"hook": {}}));
        assert_eq!((all["prune"].clone(), all["dryRun"].clone()), (json!(true), json!(true)));
        assert_eq!(
            all["resources"],
            json!([{"group": "apps", "kind": "Deployment", "namespace": "shop", "name": "web"}, {"kind": "Namespace", "name": "shop"}])
        );
    }

    #[test]
    fn requests_ask_only_for_what_the_page_reads() {
        let list = list_path();
        assert!(
            list.starts_with("/api/v1/applications?fields=metadata.resourceVersion,items.metadata.name,"),
            "{list}"
        );
        assert!(list.contains("items.status.health") && !list.contains("status.resources"), "{list}");
        // No resource version: Argo CD then sends every Application first, after subscribing.
        let stream = stream_path();
        assert!(stream.starts_with("/api/v1/stream/applications?fields=result.type,"), "{stream}");
        assert!(stream.contains("result.application.status.sync"), "{stream}");
        // What the sync journal keeps of an operation comes with it.
        assert!(stream.contains("result.application.status.operationState.syncResult.resources"), "{stream}");
        // Not in the list: it sends every Application at once.
        assert!(!list.contains("syncResult.resources") && list.contains("syncResult.revision"), "{list}");
    }

    fn app(name: &str, rv: &str, extra: Value) -> Value {
        let mut a = json!({"metadata": {"name": name, "namespace": "argocd", "resourceVersion": rv,
            "annotations": {"kubectl.kubernetes.io/last-applied-configuration": "{huge}"}},
            "spec": {"project": "shop", "destination": {"server": "https://prod.example", "namespace": "shop"}},
            "status": {"sync": {"status": "Synced", "revision": "abc"}, "health": {"status": "Healthy"},
                "resources": [{"kind": "Deployment", "name": "web"}]}});
        if let (Some(a), Some(extra)) = (a.as_object_mut(), extra.as_object()) {
            for (k, v) in extra {
                a.insert(k.clone(), v.clone());
            }
        }
        a
    }

    #[test]
    fn what_is_kept_of_an_application_is_lean() {
        let mut full = app("web", "1", json!({"operation": {"sync": {"resources": [1, 2, 3]}}}));
        full["status"]["history"] =
            json!((1..=12).map(|i| json!({"id": i, "revision": "r", "source": {"repoURL": "x"}})).collect::<Vec<_>>());
        let kept = lean(&full);
        assert_eq!(kept.pointer("/status/sync/status"), Some(&json!("Synced")));
        assert_eq!(kept["operation"], json!({}));
        assert!(kept.pointer("/status/resources").is_none());
        assert!(kept.pointer("/metadata/annotations").is_none(), "kubectl's copy is not kept");
        let history = kept.pointer("/status/history").and_then(Value::as_array).unwrap();
        assert_eq!(history.iter().map(|h| h["id"].as_i64().unwrap()).collect::<Vec<_>>(), [8, 9, 10, 11, 12]);
        assert!(history[0].get("source").is_none());
        // External URLs come from the manifests' annotations: only web links are kept.
        full["status"]["summary"] =
            json!({"externalURLs": ["https://web.example", "search-ms:query=x", "file:///x", 3]});
        assert_eq!(lean(&full).pointer("/status/summary/externalURLs"), Some(&json!(["https://web.example"])));
    }

    #[test]
    fn events_arrive_whole_however_the_bytes_are_cut() {
        let event = |ty: &str, a: &Value| format!("data: {}\n\n", json!({"result": {"type": ty, "application": a}}));
        let text = format!(
            ": heartbeat\n{}{}",
            event("ADDED", &app("wéb", "1", json!({}))),
            event("DELETED", &app("api", "2", json!({})))
        );
        let bytes = text.as_bytes();
        for cut in 1..bytes.len() {
            let mut sse = Sse::default();
            let mut got = sse.push(&bytes[..cut]).unwrap();
            got.extend(sse.push(&bytes[cut..]).unwrap());
            assert_eq!(got.len(), 2, "cut at {cut}");
            assert!(got[0].contains("wéb"), "a character split across reads stays whole");
        }
        // A data field over several lines, CRLF endings, and plain JSON lines.
        let mut sse = Sse::default();
        assert_eq!(sse.push(b"data: {\"a\":\r\ndata: 1}\r\n\r\n{\"b\":2}\n").unwrap(), ["{\"a\":\n1}", "{\"b\":2}"]);
    }

    #[test]
    fn stream_events_add_change_and_delete() {
        let mut apps = HashMap::new();
        let ev = |ty: &str, a: Value| parse_event(&json!({"result": {"type": ty, "application": a}}).to_string());
        let upsert = |r: Result<Option<Event>, String>| r.unwrap().unwrap();
        apply(&mut apps, upsert(ev("ADDED", app("web", "5", json!({"operation": {"sync": {}}})))));
        assert!(apps[&("argocd".to_string(), "web".to_string())].summary.in_progress);
        // A change replaces the Application whole: the operation that ended is gone.
        let mut modified = app("web", "6", json!({}));
        modified["status"]["sync"]["status"] = json!("OutOfSync");
        apply(&mut apps, upsert(ev("MODIFIED", modified)));
        let web = &apps[&("argocd".to_string(), "web".to_string())];
        assert_eq!((web.summary.sync.as_str(), web.summary.in_progress), ("OutOfSync", false));
        // An older one, arriving late, is left.
        apply(&mut apps, upsert(ev("MODIFIED", app("web", "4", json!({})))));
        assert_eq!(apps[&("argocd".to_string(), "web".to_string())].summary.sync, "OutOfSync");
        // A partial one (only some fields sent) reads, the others empty.
        let partial =
            json!({"metadata": {"name": "api", "namespace": "team"}, "status": {"health": {"status": "Degraded"}}});
        apply(&mut apps, upsert(ev("ADDED", partial)));
        let api = &apps[&("team".to_string(), "api".to_string())];
        assert_eq!(
            (api.summary.health.as_str(), api.summary.sync.as_str(), api.summary.project.as_str()),
            ("Degraded", "Unknown", "default")
        );
        apply(&mut apps, upsert(ev("DELETED", json!({"metadata": {"name": "web", "namespace": "argocd"}}))));
        assert_eq!(apps.len(), 1);
        // What isn't an event of an Application is skipped; an error said is an error.
        assert!(ev("BOOKMARK", app("x", "1", json!({}))).unwrap().is_none());
        assert!(parse_event("{\"result\": {}}").unwrap().is_none());
        assert!(
            parse_event("{\"error\": {\"message\": \"permission denied\"}}")
                .is_err_and(|e| e.contains("permission denied"))
        );
        assert!(parse_event("not json").is_err());
    }

    #[test]
    fn changes_sent_while_listing_land_over_the_list() {
        // The stream is open before the list is read: what it sent meanwhile is applied after
        // the list, in order, the newer of the two winning.
        let up = |a: Value| Event::Upsert(Box::new(super::ApiApp::of(&a)));
        let listed: super::Apps = [app("web", "6", json!({})), app("api", "5", json!({})), app("old", "3", json!({}))]
            .iter()
            .map(|a| {
                let a = super::ApiApp::of(a);
                (a.key(), std::sync::Arc::new(a))
            })
            .collect();
        let mut newer = app("web", "7", json!({}));
        newer["status"]["health"]["status"] = json!("Degraded");
        let sent = vec![
            // The stream's first ADDED of an Application, older than the list's: left.
            up(app("api", "4", json!({}))),
            up(newer),
            Event::Deleted { namespace: "argocd".into(), name: "old".into() },
            // Added and deleted while listing: gone.
            up(app("tmp", "8", json!({}))),
            Event::Deleted { namespace: "argocd".into(), name: "tmp".into() },
        ];
        let apps = settle(listed, sent);
        let key = |n: &str| ("argocd".to_string(), n.to_string());
        assert_eq!(apps.len(), 2);
        assert_eq!(apps[&key("web")].summary.health, "Degraded");
        assert_eq!(apps[&key("api")].value.pointer("/metadata/resourceVersion"), Some(&json!("5")));
    }
}
