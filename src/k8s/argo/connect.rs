//! Connect to Argo CD… from the app: the address people paste, how that server signs in
//! (`GET /api/v1/settings`, which needs no login), and each way in: SSO in the browser (OIDC
//! authorization code with PKCE and `state`, as `argocd login --sso` does), a local
//! account's username and password (`POST /api/v1/session`), or a token pasted. The token
//! found is kept where Settings → Argo CD keeps one (`api::save_token`, mode 0600).
//!
//! A secret only goes where it belongs: the password to the Argo CD server, the code and its
//! verifier to the identity provider's token endpoint, each over https (plain http only to
//! this machine), no redirect followed. No error holds a password, a code or a token. The
//! browser brings the sign-in back to a listener on 127.0.0.1:8085 only, which takes one
//! answer carrying the `state` it was sent, then closes, or gives up after `SSO_TIMEOUT`.

use std::time::Duration;

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use serde_json::{Value, json};
use sha2::Digest as _;

use super::server::Server;

/// Where the browser brings the sign-in back: the address Argo CD's Dex (and an OIDC
/// provider set up for the argocd CLI) accepts for the CLI.
pub const CALLBACK_PORT: u16 = 8085;
pub const REDIRECT_URI: &str = "http://localhost:8085/auth/callback";
const CALLBACK_PATH: &str = "/auth/callback";
/// The longest wait for the browser's answer.
pub const SSO_TIMEOUT: Duration = Duration::from_secs(5 * 60);
/// The longest wait for one connection to the listener to say what it wants.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
/// The largest request read by the listener, and the largest answer read from a server.
const MAX_REQUEST: usize = 16 * 1024;
const MAX_ANSWER: usize = 1024 * 1024;
const TIMEOUT: Duration = Duration::from_secs(30);
/// The scopes the argocd CLI asks for when the server names none.
const DEFAULT_SCOPES: [&str; 4] = ["openid", "profile", "email", "groups"];
/// Dex's client for the argocd CLI, and the scope Argo CD's Dex adds.
const DEX_CLI_CLIENT: &str = "argo-cd-cli";
const DEX_FEDERATED: &str = "federated:id";
/// The scope that asks for a refresh token.
const OFFLINE_ACCESS: &str = "offline_access";
/// The paths of Argo CD's web UI: an address copied from the browser ends with one of these
/// after the server's own path (`--rootpath`).
const UI_ROUTES: [&str; 11] = [
    "applications",
    "applicationsets",
    "settings",
    "user-info",
    "help",
    "login",
    "auth",
    "api",
    "projects",
    "account",
    "swagger-ui",
];

// MARK: Address

/// The Argo CD server of an address as people paste it: `argocd.example.com`, a page of its
/// web UI (`https://argocd.example.com/applications/argocd/shop?view=tree`), under a path
/// when Argo CD is served under one. https is added when no scheme is said; plain http only
/// to this machine. Anything else is refused, saying why.
pub fn normalize(pasted: &str) -> Result<Server, String> {
    // What surrounds a link copied from a page or a chat.
    let text = pasted.trim().trim_matches(|c: char| matches!(c, '<' | '>' | '"' | '\'' | '`')).trim();
    if text.is_empty() {
        return Err("Paste Argo CD's address".into());
    }
    if text.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err(format!("Not an address: {}", shown(text)));
    }
    // A query or a fragment belongs to a page of the web UI.
    let text = text.split(['#', '?']).next().unwrap_or_default();
    let (scheme, rest) = match text.split_once("://") {
        Some((s, r)) => (format!("{s}://"), r),
        None => (String::new(), text),
    };
    let (authority, path) = rest.split_once('/').unwrap_or((rest, ""));
    let root: String = path
        .split('/')
        .filter(|s| !s.is_empty())
        .take_while(|s| !UI_ROUTES.contains(s))
        .flat_map(|s| ["/", s])
        .collect();
    Server::parse(&format!("{scheme}{authority}{root}"))
}

/// `text` cut short to be shown.
fn shown(text: &str) -> String {
    let cut: String = text.chars().filter(|c| !c.is_control()).take(80).collect();
    if cut.len() < text.len() { format!("{cut}…") } else { cut }
}

/// Whether a request may go to `url`: https, or plain http to this machine.
fn allowed(url: &str) -> Result<(), String> {
    let uri = http::Uri::try_from(url).map_err(|_| format!("Not an address: {}", shown(url)))?;
    let host = uri.host().unwrap_or_default().to_ascii_lowercase();
    match uri.scheme_str() {
        Some("https") if !host.is_empty() => Ok(()),
        Some("http") if super::server::loopback(&host) => Ok(()),
        _ => Err(format!("Not sent: {} is not an https address", shown(url))),
    }
}

// MARK: How the server signs in

/// How an Argo CD server lets people sign in, as `/api/v1/settings` says.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SignIn {
    /// SSO in the browser, through Dex or an OIDC provider.
    pub sso: Option<Sso>,
    /// Local accounts may sign in with a password (`userLoginsDisabled` is not set).
    pub password: bool,
    /// The `url` set in its argocd-cm, if any.
    pub url: Option<String>,
}

/// What SSO in the browser needs, as the argocd CLI works it out.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sso {
    /// How the server names it: the OIDC provider, or Dex's connectors.
    pub name: String,
    pub issuer: String,
    /// The CLI's client id (`cliClientID`, else `clientID`; Dex's `argo-cd-cli`).
    pub client_id: String,
    pub scopes: Vec<String>,
    /// The ID token's claims asked for (`requestedIDTokenClaims`), if any.
    pub id_token_claims: Option<Value>,
}

fn text_at<'a>(v: &'a Value, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty())
}

/// How the server at `base` signs in, from its `/api/v1/settings`: OIDC first when it names
/// an issuer, else Dex when it has connectors, as the argocd CLI picks.
pub fn sign_in_of(settings: &Value, base: &str) -> SignIn {
    let url = text_at(settings, "url").map(|u| u.trim_end_matches('/').to_string());
    let password = settings.get("userLoginsDisabled").and_then(Value::as_bool) != Some(true);
    let scopes_of = |v: Option<&Value>| -> Vec<String> {
        let named: Vec<String> = v
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .filter(|s| !s.trim().is_empty())
            .collect();
        if named.is_empty() { DEFAULT_SCOPES.iter().map(|s| (*s).to_string()).collect() } else { named }
    };
    let oidc = settings.get("oidcConfig").filter(|o| o.is_object());
    let sso = if let Some(o) = oidc.filter(|o| text_at(o, "issuer").is_some()) {
        let client_id = text_at(o, "cliClientID").or_else(|| text_at(o, "clientID"));
        client_id.map(|id| Sso {
            name: text_at(o, "name").unwrap_or("SSO").to_string(),
            issuer: text_at(o, "issuer").unwrap_or_default().to_string(),
            client_id: id.to_string(),
            scopes: scopes_of(o.get("scopes")),
            id_token_claims: o.get("idTokenClaims").filter(|c| c.as_object().is_some_and(|m| !m.is_empty())).cloned(),
        })
    } else {
        let connectors: Vec<&str> = settings
            .pointer("/dexConfig/connectors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|c| text_at(c, "name").unwrap_or("Dex"))
            .collect();
        (!connectors.is_empty()).then(|| {
            let mut scopes = scopes_of(None);
            scopes.push(DEX_FEDERATED.into());
            Sso {
                name: connectors.join(", "),
                issuer: format!("{}/api/dex", url.as_deref().unwrap_or(base)),
                client_id: DEX_CLI_CLIENT.into(),
                scopes,
                id_token_claims: None,
            }
        })
    };
    SignIn { sso, password, url }
}

/// What the server answered to `/api/v1/settings`: its JSON, or why it isn't Argo CD's API.
/// A redirect, or a web page, is a login proxy in front of it (oauth2-proxy...): the API
/// can't be reached by this address, and the redirect is not followed.
pub(crate) fn settings_answer(
    name: &str,
    status: u16,
    location: Option<&str>,
    content_type: Option<&str>,
    body: &[u8],
) -> Result<Value, String> {
    let proxy = |by: Option<String>| match by {
        Some(host) => format!(
            "This Argo CD is behind a login proxy ({host}); its API can't be reached by this address. \
             Paste the address your team uses for the argocd CLI instead (`argocd login <address>`)."
        ),
        None => format!(
            "{name} answers with a web page, not Argo CD's API: a login proxy in front of it? Its API can't be \
             reached by this address: paste the address your team uses for the argocd CLI instead."
        ),
    };
    if (300..400).contains(&status) {
        let host = location.and_then(|l| http::Uri::try_from(l.trim()).ok()).and_then(|u| u.host().map(str::to_string));
        return Err(proxy(Some(host.unwrap_or_else(|| name.to_string()))));
    }
    let html = content_type.is_some_and(|t| t.to_ascii_lowercase().contains("text/html"))
        || body.iter().find(|b| !b.is_ascii_whitespace()) == Some(&b'<');
    if status == 404 {
        return Err(format!("{name} has no /api/v1/settings: is this Argo CD's address?"));
    }
    if !(200..300).contains(&status) {
        let said: String = String::from_utf8_lossy(body).chars().filter(|c| !c.is_control()).take(200).collect();
        return Err(format!("{name} answered HTTP {status} to /api/v1/settings: {}", said.trim()));
    }
    if html {
        return Err(proxy(None));
    }
    match serde_json::from_slice::<Value>(body) {
        Ok(v) if v.is_object() => Ok(v),
        _ => Err(format!("{name}'s answer isn't Argo CD's settings: is this Argo CD's address?")),
    }
}

/// What a request with no token got back.
struct Answer {
    status: u16,
    location: Option<String>,
    content_type: Option<String>,
    body: Bytes,
}

/// A request to `url` with no token (`https`, or plain http to this machine), at most
/// `MAX_ANSWER` read within `TIMEOUT`; a redirect is not followed. What fails never holds
/// one of `secrets`.
async fn plain(
    method: http::Method,
    url: &str,
    body: Option<(&str, String)>,
    secrets: &[&str],
    insecure: bool,
) -> Result<Answer, String> {
    let scrub = |text: String| secrets.iter().fold(text, |t, s| super::server::scrub(&t, s));
    allowed(url)?;
    let host = http::Uri::try_from(url).ok().and_then(|u| u.host().map(str::to_string)).unwrap_or_default();
    let url = url.to_string();
    let body = body.map(|(kind, text)| (kind.to_string(), text));
    let run = crate::k8s::run_abortable(async move {
        let fetch = async {
            let client = super::api::http_client(insecure).map_err(|e| format!("{e:#}"))?;
            let mut req = http::Request::builder()
                .method(method)
                .uri(&url)
                .header("user-agent", "recouvrance")
                .header("accept", "application/json");
            let bytes = match body {
                Some((kind, text)) => {
                    req = req.header("content-type", kind.as_str());
                    Bytes::from(text)
                }
                None => Bytes::new(),
            };
            let req = req.body(Full::new(bytes)).map_err(|e| e.to_string())?;
            let resp =
                client.request(req).await.map_err(|e| format!("can't reach {host}: {:#}", anyhow::Error::from(e)))?;
            let (parts, body) = resp.into_parts();
            let header =
                |k: http::header::HeaderName| parts.headers.get(k).and_then(|v| v.to_str().ok()).map(str::to_string);
            let read = http_body_util::Limited::new(body, MAX_ANSWER).collect().await;
            let body = read.map_err(|_| format!("{host}'s answer was cut or too large"))?.to_bytes();
            Ok(Answer {
                status: parts.status.as_u16(),
                location: header(http::header::LOCATION),
                content_type: header(http::header::CONTENT_TYPE),
                body,
            })
        };
        Ok(tokio::time::timeout(TIMEOUT, fetch).await.unwrap_or_else(|_| Err(format!("{host} did not answer"))))
    });
    run.await.unwrap_or_else(|e| Err(format!("{e:#}"))).map_err(scrub)
}

/// How the server signs in: its `/api/v1/settings`, read with no login (`insecure`: its
/// certificate not checked, as the argocd CLI's `--insecure` for it says).
pub async fn sign_in(server: Server, insecure: bool) -> Result<SignIn, String> {
    let url = format!("{}/api/v1/settings", server.base);
    let a = plain(http::Method::GET, &url, None, &[], insecure).await?;
    let settings =
        settings_answer(&server.name(), a.status, a.location.as_deref(), a.content_type.as_deref(), &a.body)?;
    Ok(sign_in_of(&settings, &server.base))
}

// MARK: Username and password

/// The token of a session answer, or why there is none.
fn session_token(name: &str, status: u16, body: &[u8]) -> Result<String, String> {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let said = text_at(&v, "message").or_else(|| text_at(&v, "error")).unwrap_or("no reason given");
        let said: String = said.chars().take(200).collect();
        return Err(match status {
            401 | 403 => format!("{name} refused this username or password ({said})"),
            300..=399 => format!("{name} answered with a redirect: the password was not sent further"),
            _ => format!("{name} answered HTTP {status}: {said}"),
        });
    }
    text_at(&v, "token").map(str::to_string).ok_or_else(|| format!("{name}'s answer holds no token"))
}

/// Sign in to a local account: the token of a new session. The password goes in the
/// request's body to that server only, and never in what fails.
pub async fn log_in(server: Server, username: String, password: String, insecure: bool) -> Result<String, String> {
    let url = format!("{}/api/v1/session", server.base);
    let body = json!({"username": username, "password": password}).to_string();
    let a = plain(http::Method::POST, &url, Some(("application/json", body)), &[&password], insecure).await?;
    session_answer(&server.name(), a.status, &a.body, &password)
}

/// The token of a session answer to a sign-in with `password`, or why there is none (never
/// with the password in it, whatever the server echoed).
fn session_answer(name: &str, status: u16, body: &[u8], password: &str) -> Result<String, String> {
    session_token(name, status, body).map_err(|e| super::server::scrub(&e, password))
}

// MARK: SSO

/// The provider's endpoints, from its discovery document.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub authorization: String,
    pub token: String,
    /// It lists `offline_access`: asked for, so it gives a refresh token (Dex, Okta, Auth0).
    pub offline: bool,
    /// Its certificate is not checked (Dex under an Argo CD reached with `--insecure`).
    pub insecure: bool,
}

/// The endpoints of the provider `issuer` from its discovery document `doc`: its issuer must
/// be the one asked, its endpoints https, and it must give codes (no implicit flow).
pub(crate) fn provider_of(doc: &Value, issuer: &str) -> Result<Provider, String> {
    let same = |a: &str, b: &str| a.trim_end_matches('/') == b.trim_end_matches('/');
    let said = text_at(doc, "issuer").unwrap_or_default();
    if !same(said, issuer) {
        return Err(format!("The identity provider calls itself {}, not {}", shown(said), shown(issuer)));
    }
    let gives_codes = doc
        .get("response_types_supported")
        .and_then(Value::as_array)
        .is_none_or(|t| t.iter().any(|t| t.as_str() == Some("code")));
    if !gives_codes {
        return Err("The identity provider only offers the implicit flow, which Recouvrance doesn't use".into());
    }
    let endpoint = |key: &str| -> Result<String, String> {
        let url = text_at(doc, key).ok_or_else(|| format!("The identity provider names no {key}"))?;
        allowed(url)?;
        Ok(url.to_string())
    };
    let offline = doc
        .get("scopes_supported")
        .and_then(Value::as_array)
        .is_some_and(|s| s.iter().any(|s| s.as_str() == Some(OFFLINE_ACCESS)));
    Ok(Provider {
        authorization: endpoint("authorization_endpoint")?,
        token: endpoint("token_endpoint")?,
        offline,
        insecure: false,
    })
}

/// Read the discovery document of `issuer` (`insecure`: its certificate not checked).
pub async fn discover(issuer: String, insecure: bool) -> Result<Provider, String> {
    let url = format!("{}/.well-known/openid-configuration", issuer.trim_end_matches('/'));
    let a = plain(http::Method::GET, &url, None, &[], insecure).await?;
    if !(200..300).contains(&a.status) {
        return Err(format!("The identity provider at {} answered HTTP {} to its discovery", shown(&issuer), a.status));
    }
    let doc: Value = serde_json::from_slice(&a.body)
        .map_err(|_| format!("The identity provider at {} sent no discovery document", shown(&issuer)))?;
    provider_of(&doc, &issuer).map(|p| Provider { insecure, ..p })
}

/// A PKCE pair (S256) and the `state` of one sign-in, random each time.
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
    pub state: String,
}

impl Pkce {
    pub fn new() -> Self {
        let verifier = format!("{}{}", uuid::Uuid::new_v4().simple(), uuid::Uuid::new_v4().simple());
        let state = uuid::Uuid::new_v4().simple().to_string();
        Self { challenge: challenge_of(&verifier), verifier, state }
    }
}

/// The S256 challenge of a verifier (RFC 7636).
fn challenge_of(verifier: &str) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(sha2::Sha256::digest(verifier.as_bytes()))
}

/// Where to send the browser: the provider's authorization endpoint, asking for a code.
pub fn authorize_url(provider: &Provider, sso: &Sso, pkce: &Pkce) -> String {
    let mut scopes = sso.scopes.clone();
    // As the argocd CLI asks, when the provider has it: else Dex gives no refresh token.
    if provider.offline && !scopes.iter().any(|s| s == OFFLINE_ACCESS) {
        scopes.push(OFFLINE_ACCESS.into());
    }
    let scope = scopes.join(" ");
    let claims = sso.id_token_claims.as_ref().map(|c| json!({"id_token": c}).to_string());
    let mut pairs = vec![
        ("response_type", "code"),
        ("client_id", sso.client_id.as_str()),
        ("redirect_uri", REDIRECT_URI),
        ("scope", scope.as_str()),
        ("state", pkce.state.as_str()),
        ("code_challenge", pkce.challenge.as_str()),
        ("code_challenge_method", "S256"),
        // As the argocd CLI asks: a refresh token, from providers that want it said.
        ("access_type", "offline"),
    ];
    if let Some(c) = &claims {
        pairs.push(("claims", c.as_str()));
    }
    let sep = if provider.authorization.contains('?') { '&' } else { '?' };
    format!("{}{sep}{}", provider.authorization, crate::prom::form_urlencoded(&pairs))
}

/// What the token endpoint gives: the id token, which Argo CD takes (as the CLI keeps it),
/// and a refresh token to renew it, when the provider gives one. Never printed.
pub struct Tokens {
    pub id_token: String,
    pub refresh_token: Option<String>,
}

impl std::fmt::Debug for Tokens {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Tokens([token], [refresh token])")
    }
}

/// The tokens of the token endpoint's answer, or why there are none.
fn tokens_of(status: u16, body: &[u8]) -> Result<Tokens, String> {
    let id_token = id_token_of(status, body)?;
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    Ok(Tokens { id_token, refresh_token: text_at(&v, "refresh_token").map(str::to_string) })
}

/// The id token of the token endpoint's answer.
fn id_token_of(status: u16, body: &[u8]) -> Result<String, String> {
    let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
    if !(200..300).contains(&status) {
        let error = text_at(&v, "error").unwrap_or("an error");
        let why = text_at(&v, "error_description").map_or_default(|d| format!(": {}", shown(d)));
        return Err(format!("The identity provider refused the sign-in ({}{why}; HTTP {status})", shown(error)));
    }
    text_at(&v, "id_token").map(str::to_string).ok_or_else(|| "The identity provider's answer has no id_token".into())
}

/// Trade the code for the tokens, with its verifier.
pub async fn exchange(provider: Provider, client_id: String, code: String, verifier: String) -> Result<Tokens, String> {
    let form = crate::prom::form_urlencoded(&[
        ("grant_type", "authorization_code"),
        ("code", &code),
        ("redirect_uri", REDIRECT_URI),
        ("client_id", &client_id),
        ("code_verifier", &verifier),
    ]);
    let a = plain(
        http::Method::POST,
        &provider.token,
        Some(("application/x-www-form-urlencoded", form)),
        &[&code, &verifier],
        provider.insecure,
    )
    .await?;
    if (300..400).contains(&a.status) {
        return Err("The identity provider's token endpoint answered with a redirect: not followed".into());
    }
    tokens_of(a.status, &a.body)
}

/// `%xx` and `+` of a query value decoded.
fn decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = (b[i] == b'%').then(|| s.get(i + 1..i + 3)).flatten().and_then(|h| u8::from_str_radix(h, 16).ok());
        match (b[i], hex) {
            (b'+', _) => out.push(b' '),
            (_, Some(x)) => {
                out.push(x);
                i += 2;
            }
            (c, None) => out.push(c),
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Whether two texts are the same, in a time that doesn't depend on where they differ.
fn same_secret(a: &str, b: &str) -> bool {
    a.len() == b.len() && a.bytes().zip(b.bytes()).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// What the listener answers a request with.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reply {
    pub status: u16,
    pub page: &'static str,
}

const PAGE_DONE: &str = "Signed in: you can close this tab and go back to Recouvrance.";
const PAGE_FAILED: &str = "The sign-in failed: Recouvrance says why.";
const PAGE_UNKNOWN: &str = "This is not the sign-in Recouvrance is waiting for.";
const PAGE_GONE: &str = "This sign-in is over.";
const PAGE_NOT_HERE: &str = "Not found.";

/// The listener's judge: the browser's answers, one at a time, until one carries the
/// `state` sent. That one ends the sign-in, with its code or the error the provider gave;
/// any other request leaves it waiting. Nothing after the end is taken.
pub(crate) struct Callback {
    state: String,
    done: bool,
}

impl Callback {
    pub fn new(state: String) -> Self {
        Self { state, done: false }
    }

    /// Judge a request by its head (request line and headers): the reply, and how the
    /// sign-in ended when this request ends it.
    pub fn handle(&mut self, head: &str) -> (Reply, Option<Result<String, String>>) {
        let reply = |status, page| Reply { status, page };
        let mut lines = head.lines();
        let mut first = lines.next().unwrap_or_default().split(' ');
        let (method, target) = (first.next().unwrap_or_default(), first.next().unwrap_or_default());
        let host = lines
            .filter_map(|l| l.split_once(':'))
            .find(|(k, _)| k.trim().eq_ignore_ascii_case("host"))
            .map(|(_, v)| v.trim().to_ascii_lowercase());
        let (path, query) = target.split_once('?').unwrap_or((target, ""));
        // Only the browser coming back here: not a page of another site (DNS rebinding).
        let local_host = host.as_deref().is_some_and(|h| {
            [format!("localhost:{CALLBACK_PORT}"), format!("127.0.0.1:{CALLBACK_PORT}")].contains(&h.to_string())
        });
        if method != "GET" || path != CALLBACK_PATH || !local_host {
            return (reply(404, PAGE_NOT_HERE), None);
        }
        if self.done {
            return (reply(410, PAGE_GONE), None);
        }
        let param = |name: &str| {
            query.split('&').filter_map(|p| p.split_once('=')).find(|(k, _)| *k == name).map(|(_, v)| decode(v))
        };
        if !param("state").is_some_and(|s| same_secret(&s, &self.state)) {
            return (reply(400, PAGE_UNKNOWN), None);
        }
        self.done = true;
        if let Some(error) = param("error") {
            let why = param("error_description").map_or_default(|d| format!(": {}", shown(&d)));
            return (reply(400, PAGE_FAILED), Some(Err(format!("The sign-in was refused ({}{why})", shown(&error)))));
        }
        match param("code").filter(|c| !c.is_empty()) {
            Some(code) => (reply(200, PAGE_DONE), Some(Ok(code))),
            None => (reply(400, PAGE_FAILED), Some(Err("The sign-in came back with no code".into()))),
        }
    }
}

/// Listen for the browser's answer on 127.0.0.1:8085 only. Taken: say so.
pub async fn listen() -> Result<tokio::net::TcpListener, String> {
    let bound = crate::k8s::run(async {
        Ok(tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, CALLBACK_PORT)).await)
    })
    .await
    .map_err(|e| format!("{e:#}"))?;
    bound.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!(
                "Port {CALLBACK_PORT} of this machine is in use (by another `argocd login --sso`?): the sign-in comes \
                 back there. Close what uses it, then try again."
            )
        } else {
            format!("Can't listen on 127.0.0.1:{CALLBACK_PORT}: {e}")
        }
    })
}

/// The head of a request (up to its blank line), at most `MAX_REQUEST` bytes.
async fn read_head(sock: &mut tokio::net::TcpStream) -> Option<String> {
    use tokio::io::AsyncReadExt as _;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 2048];
    while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
        let n = sock.read(&mut chunk).await.ok()?;
        if n == 0 || buf.len() + n > MAX_REQUEST {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    }
    Some(String::from_utf8_lossy(&buf).into_owned())
}

/// Wait for the browser's answer on `listener`: the code, or why there is none. The
/// listener closes when this returns (one answer), or after `timeout`.
pub async fn wait_for_code(
    listener: tokio::net::TcpListener,
    state: String,
    timeout: Duration,
) -> Result<String, String> {
    use tokio::io::AsyncWriteExt as _;
    let wait = async move {
        let mut judge = Callback::new(state);
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let Ok(accepted) = tokio::time::timeout_at(deadline, listener.accept()).await else {
                return Err(format!(
                    "No answer from the browser within {} minutes: try again",
                    timeout.as_secs().div_ceil(60)
                ));
            };
            let Ok((mut sock, _)) = accepted else { continue };
            let Ok(Some(head)) = tokio::time::timeout(REQUEST_TIMEOUT, read_head(&mut sock)).await else { continue };
            let (reply, ended) = judge.handle(&head);
            let reason = match reply.status {
                200 => "OK",
                400 => "Bad Request",
                410 => "Gone",
                _ => "Not Found",
            };
            let text = format!(
                "HTTP/1.1 {} {reason}\r\nContent-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n\
                 Cache-Control: no-store\r\nReferrer-Policy: no-referrer\r\nConnection: close\r\n\r\n{}",
                reply.status,
                reply.page.len(),
                reply.page
            );
            let _ = tokio::time::timeout(REQUEST_TIMEOUT, sock.write_all(text.as_bytes())).await;
            let _ = sock.shutdown().await;
            if let Some(ended) = ended {
                return ended;
            }
        }
    };
    crate::k8s::run_abortable(async move { Ok(wait.await) }).await.unwrap_or_else(|e| Err(format!("{e:#}")))
}

// MARK: Saving

/// Settings for a server connected: listed in Settings → Argo CD, and read.
pub(crate) fn connect_in(s: &mut crate::settings::Settings, base: &str) {
    if !s.argocd_api_servers.iter().any(|t| Server::parse(t).is_ok_and(|x| x.base == base)) {
        s.argocd_api_servers.push(base.to_string());
    }
    s.argocd_api_off.retain(|b| b != base);
}

/// Keep the token for `server` (0600), what renews it when the sign-in gave a refresh token
/// (else none: a password's or a pasted token isn't renewed), and its settings
/// (`connect_in`). The pages read the servers again.
pub fn save(server: &Server, token: &str, refresh: Option<Refresh>) -> Result<(), String> {
    {
        let _held = credentials();
        super::api::write_token(&server.base, token).map_err(|e| format!("Could not save the token: {e}"))?;
        save_refresh_locked(&server.base, refresh).map_err(|e| format!("Could not save the refresh token: {e}"))?;
    }
    forget_renewal(&server.base);
    crate::settings::update(|s| connect_in(s, &server.base));
    super::api::reload();
    Ok(())
}

// MARK: Renewal

/// Where the refresh tokens of the SSO sign-ins made here are kept, by server (mode 0600).
/// The argocd CLI's refresh token is never used: a provider that rotates them on use would
/// leave the CLI's own one invalid.
const REFRESH_FILE: &str = "argocd-api-refresh.json";
/// A login is renewed this long before it expires.
const RENEW_BEFORE: i64 = 30;
/// Renewals of one server are at least this far apart.
const RENEW_EVERY: Duration = Duration::from_secs(20);

/// What renews a server's login: the provider's token endpoint, the client and the refresh
/// token (never printed).
#[derive(Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct Refresh {
    pub token_endpoint: String,
    pub client_id: String,
    pub refresh_token: String,
    /// The token endpoint's certificate is not checked (Dex under an `--insecure` server).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub insecure: bool,
}

impl std::fmt::Debug for Refresh {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Refresh({}, {}, [token])", self.token_endpoint, self.client_id)
    }
}

fn refresh_path() -> std::path::PathBuf {
    crate::paths::config_dir().join(REFRESH_FILE)
}

/// The refresh tokens kept, by server; none when there is no file. One that doesn't read is
/// an error: nothing is renewed (the login then expires and says so).
fn read_refresh() -> Result<std::collections::BTreeMap<String, Refresh>, String> {
    match std::fs::read(refresh_path()) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(|e| format!("{}: {e}", refresh_path().display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
        Err(e) => Err(format!("{}: {e}", refresh_path().display())),
    }
}

/// One lock for the files of tokens and refresh tokens, each read, changed and written whole:
/// two saves never lose one another's change (a rotated refresh token above all).
static CREDENTIALS: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub(super) fn credentials() -> std::sync::MutexGuard<'static, ()> {
    CREDENTIALS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Keep what renews the login to `base` (None: forget it), readable only by the user, under
/// the credentials lock (`credentials`). A file that doesn't read is kept aside first.
pub(super) fn save_refresh_locked(base: &str, refresh: Option<Refresh>) -> std::io::Result<()> {
    let mut all = read_refresh().unwrap_or_else(|_| {
        crate::settings::keep_aside(&refresh_path());
        Default::default()
    });
    let changed = match refresh {
        Some(r) => all.insert(base.to_string(), r.clone()).as_ref() != Some(&r),
        None => all.remove(base).is_some(),
    };
    if !changed {
        return Ok(());
    }
    if all.is_empty() {
        return match std::fs::remove_file(refresh_path()) {
            Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        };
    }
    let text = serde_json::to_vec_pretty(&all).map_err(std::io::Error::other)?;
    crate::private_files::write_atomic(&refresh_path(), &text)
}

/// Whether the login to `base` set here can be renewed (a refresh token is kept for it).
pub fn renewable(base: &str) -> bool {
    read_refresh().is_ok_and(|all| all.contains_key(base))
}

/// A renewal that failed: why, and whether the login is dead (the provider refused the
/// refresh token, or none is kept) or the renewal may work later (a network error).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Failed {
    pub why: String,
    pub fatal: bool,
}

impl Failed {
    fn fatal(why: impl Into<String>) -> Self {
        Self { why: why.into(), fatal: true }
    }

    fn later(why: impl Into<String>) -> Self {
        Self { why: why.into(), fatal: false }
    }
}

/// Each server's last renewal: when it started, whether it still runs, how it failed.
#[derive(Clone)]
struct Renewal {
    at: std::time::Instant,
    running: bool,
    failed: Option<Failed>,
}

type Renewals = std::collections::HashMap<String, Renewal>;
static RENEWALS: std::sync::LazyLock<std::sync::Mutex<Renewals>> = std::sync::LazyLock::new(Default::default);

fn renewals() -> std::sync::MutexGuard<'static, Renewals> {
    RENEWALS.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn forget_renewal(base: &str) {
    renewals().remove(base);
}

/// What to say of the login to `base` (named `name`) renewed here, when its token isn't
/// usable: renewing it now, or why renewing failed. None when neither (it is renewed while
/// a page reads that server).
pub fn renewal_note(name: &str, base: &str) -> Option<String> {
    let r = renewals().get(base).cloned()?;
    if r.running {
        return Some(format!("Renewing the login to {name}…"));
    }
    r.failed.map(|f| {
        let then = if f.fatal { "Reconnect" } else { "it is tried again" };
        format!("The login to {name} has expired, and renewing it failed ({}): {then}", f.why)
    })
}

/// Whether renewing the login to `base` failed for good (until Reconnect).
pub fn renewal_dead(base: &str) -> bool {
    renewals().get(base).is_some_and(|r| r.failed.as_ref().is_some_and(|f| f.fatal))
}

/// Seconds to wait before renewing a token expiring at `exp` (now: `now`): `RENEW_BEFORE`
/// before it, at once when that is past. None for a token with no `exp`.
pub(super) fn renew_in(exp: Option<i64>, now: i64) -> Option<u64> {
    exp.map(|exp| u64::try_from(exp - RENEW_BEFORE - now).unwrap_or(0))
}

/// The tokens of a refresh answer: the id token and the refresh token, rotated or not. A
/// refresh token refused (`invalid_grant`...) is fatal; a server error may pass.
fn renewal_answer(status: u16, body: &[u8], kept: &str) -> Result<Tokens, Failed> {
    match tokens_of(status, body) {
        Ok(mut t) => {
            if t.refresh_token.is_none() {
                t.refresh_token = Some(kept.to_string());
            }
            Ok(t)
        }
        Err(e) => {
            let why = super::server::scrub(&e.replace("refused the sign-in", "refused to renew the login"), kept);
            let v: Value = serde_json::from_slice(body).unwrap_or(Value::Null);
            let dead = matches!(text_at(&v, "error"), Some("invalid_grant" | "invalid_client" | "unauthorized_client"))
                || (400..500).contains(&status);
            Err(if dead { Failed::fatal(why) } else { Failed::later(why) })
        }
    }
}

/// Renew the login to `base` with the refresh token kept for it, unless one runs, ran just
/// now, or failed for good. Once started it runs to its end, even when nothing waits for it
/// any more: the provider may have rotated the refresh token, which must then be kept. The
/// pages read the servers again (without listing anything again).
pub async fn renew(base: String) -> Result<(), Failed> {
    {
        let mut all = renewals();
        match all.get(&base) {
            Some(r) if r.running => return Ok(()),
            Some(Renewal { failed: Some(f), .. }) if f.fatal => return Err(f.clone()),
            Some(Renewal { at, failed: None, .. }) if at.elapsed() < RENEW_EVERY => return Ok(()),
            _ => {}
        }
        all.insert(base.clone(), Renewal { at: std::time::Instant::now(), running: true, failed: None });
    }
    let (tx, rx) = tokio::sync::oneshot::channel();
    crate::k8s::spawn_detached(async move {
        let result = renew_now(&base).await;
        let mut all = renewals();
        // Reconnected meanwhile: its own state stands.
        if all.get(&base).is_some_and(|r| r.running) {
            all.insert(base, Renewal { at: std::time::Instant::now(), running: false, failed: result.clone().err() });
        }
        drop(all);
        super::api::renewed();
        let _ = tx.send(result);
    });
    rx.await.unwrap_or_else(|_| Err(Failed::later("the renewal stopped")))
}

/// Whether a renewal started with `started` may keep what it got: only while the entry
/// kept is still that one. Else the login changed meanwhile (Reconnect, a token set or
/// forgotten, another Recouvrance that renewed first and rotated it): that one wins, and a
/// refusal of the old refresh token is no failure.
fn still_used(started: &Refresh, now: Option<&Refresh>) -> bool {
    now == Some(started)
}

/// The entry kept for `base` now, under the credentials lock.
fn kept_refresh(base: &str) -> Result<Option<Refresh>, Failed> {
    let _held = credentials();
    read_refresh().map(|mut all| all.remove(base)).map_err(Failed::fatal)
}

async fn renew_now(base: &str) -> Result<(), Failed> {
    let kept = kept_refresh(base)?.ok_or_else(|| Failed::fatal("no refresh token is kept for it"))?;
    let form = crate::prom::form_urlencoded(&[
        ("grant_type", "refresh_token"),
        ("refresh_token", &kept.refresh_token),
        ("client_id", &kept.client_id),
    ]);
    let secret = kept.refresh_token.clone();
    let body = Some(("application/x-www-form-urlencoded", form));
    let a = plain(http::Method::POST, &kept.token_endpoint, body, &[&secret], kept.insecure)
        .await
        .map_err(Failed::later)?;
    let renewed = if (300..400).contains(&a.status) {
        Err(Failed::fatal("the token endpoint answered with a redirect: not followed"))
    } else {
        renewal_answer(a.status, &a.body, &secret)
    };
    // Saved only if what renewed is still the one used: a Reconnect, a token set or forgot,
    // or another Recouvrance that renewed it first while this one waited, wins.
    let _held = credentials();
    let now = read_refresh().map_err(Failed::fatal)?.remove(base);
    if !still_used(&kept, now.as_ref()) {
        return Ok(());
    }
    let t = renewed?;
    let rotated = Refresh { refresh_token: t.refresh_token.unwrap_or(secret), ..kept };
    save_refresh_locked(base, Some(rotated))
        .map_err(|e| Failed::later(format!("could not keep the new refresh token: {e}")))?;
    super::api::write_token(base, &t.id_token).map_err(|e| Failed::later(format!("could not keep the new token: {e}")))
}

#[cfg(test)]
mod tests {
    use super::{
        Callback, Pkce, Provider, Sso, UI_ROUTES, authorize_url, challenge_of, connect_in, decode, id_token_of,
        normalize, provider_of, session_token, settings_answer, sign_in_of,
    };
    use proptest::prelude::*;
    use serde_json::json;

    #[test]
    fn what_people_paste_reads_as_the_server() {
        let base = |t: &str| normalize(t).map(|s| s.base);
        assert_eq!(base("argocd.example.com").unwrap(), "https://argocd.example.com");
        assert_eq!(
            base(" <https://ArgoCD.example.com/applications/argocd/shop?view=tree#x> ").unwrap(),
            "https://argocd.example.com"
        );
        assert_eq!(base("https://example.com/argo-cd/applications").unwrap(), "https://example.com/argo-cd");
        assert_eq!(base("https://example.com:8443/argo/settings/repos/").unwrap(), "https://example.com:8443/argo");
        assert_eq!(base("http://localhost:8080/applications").unwrap(), "http://localhost:8080");
        assert!(base("http://argocd.example.com").is_err());
        assert!(base("https://me@argocd.example.com").is_err());
        assert!(base("argocd login argocd.example.com").is_err());
        assert!(base("").is_err());
        assert!(base("ftp://argocd.example.com").is_err());
    }

    proptest! {
        /// Any address of a server, as copied from anywhere, reads as its canonical base.
        #[test]
        fn a_pasted_address_reads_as_its_base(
            host in "[a-z][a-z0-9-]{0,10}(\\.[a-z][a-z0-9-]{0,10}){0,3}",
            upper in any::<bool>(),
            scheme in any::<bool>(),
            port in prop::option::of(1u16..),
            root in prop::collection::vec("[a-z0-9][a-z0-9_-]{0,8}", 0..3),
            page in prop::option::of(prop::sample::select(vec![
                "/applications", "/applications/argocd/shop", "/settings/repos", "/login", "/user-info", "/applicationsets/x",
            ])),
            query in prop::option::of("[a-z0-9=&]{0,12}"),
            fragment in prop::option::of("[a-z0-9/]{0,12}"),
            slash in any::<bool>(),
            pad in "[ \t\n]{0,3}",
            wrap in prop::sample::select(vec![("", ""), ("<", ">"), ("\"", "\""), ("`", "`")]),
        ) {
            prop_assume!(root.iter().all(|s| !UI_ROUTES.contains(&s.as_str())));
            let shown_host = if upper { host.to_uppercase() } else { host.clone() };
            let port_text = port.map_or_default(|p| format!(":{p}"));
            let root_path: String = root.iter().flat_map(|s| ["/", s.as_str()]).collect();
            let mut text = format!("{}{shown_host}{port_text}{root_path}{}", if scheme { "https://" } else { "" }, page.unwrap_or(""));
            if slash { text.push('/'); }
            if let Some(q) = &query { text.push('?'); text.push_str(q); }
            if let Some(f) = &fragment { text.push('#'); text.push_str(f); }
            let pasted = format!("{pad}{}{text}{}{pad}", wrap.0, wrap.1);
            let kept_port = port.filter(|p| *p != 443).map_or_default(|p| format!(":{p}"));
            let expected = format!("https://{host}{kept_port}{root_path}");
            prop_assert_eq!(normalize(&pasted).map(|s| s.base), Ok(expected));
        }

        /// Whatever is pasted gives a base a token may go to, or a refusal: https or this
        /// machine, no user, query, fragment or trailing slash; reading it again gives it.
        #[test]
        fn whatever_is_pasted_is_a_safe_base_or_a_refusal(text in ".{0,80}") {
            if let Ok(s) = normalize(&text) {
                let b = s.base;
                let local = ["http://localhost", "http://127.", "http://[::1]"].iter().any(|p| b.starts_with(p));
                prop_assert!(b.starts_with("https://") || local, "{}", b);
                let authority = b.split_once("://").map_or("", |(_, r)| r.split('/').next().unwrap_or_default());
                prop_assert!(!authority.contains('@'), "{}", b);
                prop_assert!(!b.contains(['?', '#', ' ']) && !b.ends_with('/'), "{}", b);
                prop_assert_eq!(normalize(&b).map(|s| s.base), Ok(b));
            }
        }

        /// Plain http to another machine is refused, wherever it is pasted from.
        #[test]
        fn plain_http_goes_only_to_this_machine(host in "[a-z][a-z0-9-]{0,10}\\.[a-z]{2,5}", page in "(/applications)?") {
            let pasted = format!("http://{host}{page}");
            prop_assert!(normalize(&pasted).is_err(), "{}", pasted);
        }

        /// Whatever the provider says when it refuses, the code and verifier sent are not in
        /// the error, and neither is a password in a session's refusal.
        #[test]
        fn a_secret_never_shows_in_an_error(secret in "[A-Za-z0-9._-]{12,40}", before in "[a-z ]{0,10}") {
            let body = json!({"error": format!("{before}{secret}"), "message": format!("{before}{secret}")}).to_string();
            let said = super::session_answer("argocd", 401, body.as_bytes(), &secret).unwrap_err();
            prop_assert!(!said.contains(&secret), "{}", said);
            let renewal = super::renewal_answer(400, body.as_bytes(), &secret).unwrap_err();
            prop_assert!(!renewal.why.contains(&secret), "{}", renewal.why);
        }
    }

    #[test]
    fn sso_via_dex_uses_the_cli_client_under_the_servers_url() {
        let s = json!({"url": "https://argocd.example.com/", "dexConfig": {"connectors": [{"name": "GitHub", "type": "github"}]}});
        let sign = sign_in_of(&s, "https://other.example.com");
        let sso = sign.sso.unwrap();
        assert_eq!(sso.issuer, "https://argocd.example.com/api/dex");
        assert_eq!(sso.client_id, "argo-cd-cli");
        assert_eq!(sso.name, "GitHub");
        assert!(sso.scopes.contains(&"federated:id".to_string()) && sso.scopes.contains(&"openid".to_string()));
        assert!(sign.password);
        // No url set: the address pasted.
        let s = json!({"dexConfig": {"connectors": [{"name": "Google"}]}});
        assert_eq!(sign_in_of(&s, "https://a.example.com").sso.unwrap().issuer, "https://a.example.com/api/dex");
        // Dex without connectors: no SSO.
        assert_eq!(sign_in_of(&json!({"dexConfig": {}}), "https://a").sso, None);
    }

    #[test]
    fn sso_via_oidc_prefers_the_cli_client_id() {
        let s = json!({"oidcConfig": {"name": "Okta", "issuer": "https://okta.example.com", "clientID": "web",
            "cliClientID": "cli", "scopes": ["openid", "groups"], "idTokenClaims": {"groups": {"essential": true}}},
            "dexConfig": {"connectors": [{"name": "GitHub"}]}, "userLoginsDisabled": true});
        let sign = sign_in_of(&s, "https://a");
        let sso = sign.sso.unwrap();
        assert_eq!((sso.name.as_str(), sso.client_id.as_str()), ("Okta", "cli"));
        assert_eq!(sso.scopes, ["openid", "groups"]);
        assert!(sso.id_token_claims.is_some());
        // Local login disabled: no password offered.
        assert!(!sign.password);
        let s = json!({"oidcConfig": {"issuer": "https://okta.example.com", "clientID": "web"}});
        let sso = sign_in_of(&s, "https://a").sso.unwrap();
        assert_eq!(sso.client_id, "web");
        assert_eq!(sso.scopes, ["openid", "profile", "email", "groups"]);
        // Nothing set: a password (or a token) only.
        let none = sign_in_of(&json!({"url": "https://a"}), "https://a");
        assert_eq!((none.sso, none.password), (None, true));
    }

    #[test]
    fn a_login_proxy_is_said_and_its_redirect_not_followed() {
        let proxied = settings_answer(
            "argocd.example.com",
            302,
            Some("https://sso.example.com/oauth2/start?rd=https%3A%2F%2Fargocd.example.com"),
            Some("text/html"),
            b"<a href=\"...\">Found</a>",
        )
        .unwrap_err();
        assert!(proxied.contains("behind a login proxy (sso.example.com)"), "{proxied}");
        assert!(proxied.contains("the address your team uses for the argocd CLI"), "{proxied}");
        // A login page answered with 200.
        let page =
            settings_answer("a", 200, None, Some("text/html; charset=utf-8"), b"<!doctype html><html>").unwrap_err();
        assert!(page.contains("login proxy"), "{page}");
        let page = settings_answer("a", 200, None, None, b"  <html>").unwrap_err();
        assert!(page.contains("login proxy"), "{page}");
        assert!(settings_answer("a", 404, None, None, b"").unwrap_err().contains("no /api/v1/settings"));
        assert!(settings_answer("a", 200, None, Some("application/json"), b"[1]").is_err());
        assert!(settings_answer("a", 200, None, Some("application/json"), br#"{"url":"x"}"#).is_ok());
    }

    #[test]
    fn the_pkce_challenge_is_rfc_7636s() {
        assert_eq!(
            challenge_of("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        let (a, b) = (Pkce::new(), Pkce::new());
        assert!(a.verifier.len() >= 43 && a.verifier != b.verifier && a.state != b.state);
    }

    fn sso() -> Sso {
        Sso {
            name: "Dex".into(),
            issuer: "https://a/api/dex".into(),
            client_id: "argo-cd-cli".into(),
            scopes: vec!["openid".into(), "groups".into()],
            id_token_claims: None,
        }
    }

    #[test]
    fn the_browser_is_sent_with_a_s256_challenge_and_the_state() {
        let p = Provider {
            authorization: "https://a/api/dex/auth".into(),
            token: "https://a/api/dex/token".into(),
            offline: false,
            insecure: false,
        };
        let pkce = Pkce::new();
        let url = authorize_url(&p, &sso(), &pkce);
        assert!(url.starts_with("https://a/api/dex/auth?response_type=code&"), "{url}");
        assert!(
            url.contains(&format!("state={}", pkce.state))
                && url.contains(&format!("code_challenge={}", pkce.challenge))
        );
        assert!(url.contains("code_challenge_method=S256") && url.contains("scope=openid%20groups"));
        assert!(url.contains("redirect_uri=http%3A%2F%2Flocalhost%3A8085%2Fauth%2Fcallback"));
        // The verifier stays here.
        assert!(!url.contains(&pkce.verifier));
    }

    #[test]
    fn the_provider_must_be_the_one_asked_and_give_codes() {
        let doc = json!({"issuer": "https://a/api/dex", "authorization_endpoint": "https://a/api/dex/auth",
            "token_endpoint": "https://a/api/dex/token", "response_types_supported": ["code", "id_token"]});
        assert!(provider_of(&doc, "https://a/api/dex/").is_ok());
        assert!(provider_of(&doc, "https://b/api/dex").is_err());
        let mut implicit = doc.clone();
        implicit["response_types_supported"] = json!(["id_token"]);
        assert!(provider_of(&implicit, "https://a/api/dex").is_err());
        let mut plain = doc;
        plain["token_endpoint"] = json!("http://idp.example.com/token");
        assert!(provider_of(&plain, "https://a/api/dex").is_err());
    }

    #[test]
    fn the_token_endpoint_gives_the_id_token() {
        assert_eq!(id_token_of(200, br#"{"id_token": "t", "access_token": "x"}"#).unwrap(), "t");
        let t = super::tokens_of(200, br#"{"id_token": "t", "refresh_token": "r"}"#).unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("r"));
        assert!(id_token_of(200, br#"{"access_token": "x"}"#).is_err());
        let e = id_token_of(400, br#"{"error": "invalid_grant", "error_description": "expired"}"#).unwrap_err();
        assert!(e.contains("invalid_grant: expired"), "{e}");
    }

    const HOST: &str = "Host: localhost:8085\r\n";

    fn get(target: &str) -> String {
        format!("GET {target} HTTP/1.1\r\n{HOST}\r\n")
    }

    #[test]
    fn the_callback_takes_one_answer_with_the_state_sent() {
        let mut cb = Callback::new("s3cret".into());
        // Other paths and a wrong state leave it waiting.
        assert_eq!(cb.handle(&get("/favicon.ico")).0.status, 404);
        let (reply, ended) = cb.handle(&get("/auth/callback?code=c&state=other"));
        assert_eq!((reply.status, ended), (400, None));
        assert_eq!(cb.handle(&get("/auth/callback?code=c")).1, None);
        // Another site's page can't answer (DNS rebinding).
        let rebound = "GET /auth/callback?code=c&state=s3cret HTTP/1.1\r\nHost: evil.example.com:8085\r\n\r\n";
        assert_eq!(cb.handle(rebound).1, None);
        let (reply, ended) = cb.handle(&get("/auth/callback?code=the%2Fcode&state=s3cret"));
        assert_eq!((reply.status, ended), (200, Some(Ok("the/code".to_string()))));
        // A second request, even right, takes nothing.
        let (reply, ended) = cb.handle(&get("/auth/callback?code=other&state=s3cret"));
        assert_eq!((reply.status, ended), (410, None));
    }

    #[test]
    fn the_callback_says_the_providers_refusal() {
        let mut cb = Callback::new("s".into());
        let (_, ended) = cb.handle(&get("/auth/callback?state=s&error=access_denied&error_description=no+way"));
        let why = ended.unwrap().unwrap_err();
        assert!(why.contains("access_denied: no way"), "{why}");
        // An error with the wrong state ends nothing.
        let mut cb = Callback::new("s".into());
        assert_eq!(cb.handle(&get("/auth/callback?state=x&error=access_denied")).1, None);
        // POST is not the browser's answer.
        let post = format!("POST /auth/callback?code=c&state=s HTTP/1.1\r\n{HOST}\r\n");
        assert_eq!(cb.handle(&post).1, None);
    }

    #[test]
    fn a_login_is_renewed_before_it_expires() {
        use super::{renew_in, renewal_answer as renewed};
        // A 2-minute token: renewed 30 s before its end; one past or nearly so, at once.
        assert_eq!(renew_in(Some(1120), 1000), Some(90));
        assert_eq!(renew_in(Some(1010), 1000), Some(0));
        assert_eq!(renew_in(Some(900), 1000), Some(0));
        assert_eq!(renew_in(None, 1000), None);
        // A rotated refresh token replaces the kept one; none sent keeps it.
        let t = renewed(200, br#"{"id_token": "new", "refresh_token": "rotated"}"#, "old").unwrap();
        assert_eq!((t.id_token.as_str(), t.refresh_token.as_deref()), ("new", Some("rotated")));
        let t = renewed(200, br#"{"id_token": "new"}"#, "old").unwrap();
        assert_eq!(t.refresh_token.as_deref(), Some("old"));
        // A refused refresh token is dead (until Reconnect); a server error may pass.
        let e = renewed(400, br#"{"error": "invalid_grant", "error_description": "token old revoked"}"#, "old")
            .err()
            .unwrap();
        assert!(e.fatal && e.why.contains("refused to renew") && !e.why.contains("old"), "{e:?}");
        assert!(renewed(401, br#"{"error": "invalid_client"}"#, "old").err().unwrap().fatal);
        assert!(!renewed(503, b"busy", "old").err().unwrap().fatal);
    }

    #[test]
    fn a_renewal_keeps_what_it_got_only_while_its_login_is_still_used() {
        use super::{Refresh, still_used};
        let r = |t: &str| Refresh {
            token_endpoint: "https://idp/token".into(),
            client_id: "cli".into(),
            refresh_token: t.into(),
            insecure: false,
        };
        assert!(still_used(&r("a"), Some(&r("a"))));
        // Forgotten, a token set by hand (no entry), Reconnect or another instance (another one).
        assert!(!still_used(&r("a"), None));
        assert!(!still_used(&r("a"), Some(&r("b"))));
    }

    #[test]
    fn offline_access_is_asked_when_the_provider_lists_it() {
        let dex = json!({"issuer": "https://a/api/dex", "authorization_endpoint": "https://a/api/dex/auth",
            "token_endpoint": "https://a/api/dex/token", "scopes_supported": ["openid", "groups", "offline_access"]});
        let p = provider_of(&dex, "https://a/api/dex").unwrap();
        assert!(p.offline);
        let url = authorize_url(&p, &sso(), &Pkce::new());
        assert!(url.contains("scope=openid%20groups%20offline_access"), "{url}");
        // GitLab doesn't list it, and gives a refresh token anyway: not asked.
        let gitlab = json!({"issuer": "https://gitlab.example.com", "authorization_endpoint": "https://gitlab.example.com/oauth/authorize",
            "token_endpoint": "https://gitlab.example.com/oauth/token", "scopes_supported": ["openid", "profile", "read_user"]});
        let p = provider_of(&gitlab, "https://gitlab.example.com").unwrap();
        assert!(!p.offline);
        assert!(authorize_url(&p, &sso(), &Pkce::new()).contains("scope=openid%20groups&"));
    }

    #[test]
    fn a_refresh_token_never_prints() {
        let r = super::Refresh {
            token_endpoint: "https://idp/token".into(),
            client_id: "cli".into(),
            refresh_token: "s3cret".into(),
            insecure: false,
        };
        assert!(!format!("{r:?}").contains("s3cret"));
    }

    #[test]
    fn query_values_are_decoded() {
        assert_eq!(decode("a%20b+c%2F"), "a b c/");
        assert_eq!(decode("100%"), "100%");
        assert_eq!(decode("%zz"), "%zz");
    }

    #[test]
    fn a_password_never_shows_in_the_sessions_refusal() {
        let e = session_token("argocd", 401, br#"{"error": "Invalid username or password"}"#).unwrap_err();
        assert!(e.contains("refused this username or password"), "{e}");
        assert_eq!(session_token("argocd", 200, br#"{"token": "t"}"#).unwrap(), "t");
        assert!(session_token("argocd", 302, b"").unwrap_err().contains("redirect"));
    }

    #[test]
    fn connecting_lists_the_server_once_and_reads_it() {
        let mut s = crate::settings::Settings::default();
        s.argocd_api_off.push("https://argocd.example.com".into());
        s.argocd_api_servers.push("ArgoCD.example.com/".into());
        connect_in(&mut s, "https://argocd.example.com");
        assert_eq!(s.argocd_api_servers, ["ArgoCD.example.com/"]);
        assert!(s.argocd_api_off.is_empty());
        connect_in(&mut s, "https://other.example.com");
        assert_eq!(s.argocd_api_servers.len(), 2);
        // The hubs' setting is left alone.
        assert!(s.argocd_servers.is_empty());
    }
}
