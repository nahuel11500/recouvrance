//! Whether an image runs its repository's newest release tag: the tags read from its registry
//! (the OCI distribution API, `GET /v2/<repository>/tags/list`), compared as numbers with the
//! one deployed (`k8s::argo::release`).
//!
//! The credentials are those of the user's Docker config (`$DOCKER_CONFIG/config.json`, else
//! `~/.docker/config.json`): an `auths` entry, else a credential helper (`credHelpers`, then
//! `credsStore`, run as `docker-credential-<helper> get`). They are only read. A registry with
//! none isn't asked: no credentials, no check. The registry's token flow is followed (a 401
//! with `WWW-Authenticate: Bearer realm=…` gets a pull token from its realm), only https is
//! used, and a credential never shows in an error.
//!
//! Each answer is kept 5 minutes. When the registry can't be read, the last answer stays,
//! with why. The date of a newer tag comes from its image's config (`created`), when it can be
//! read.

use std::collections::HashMap;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use base64::Engine as _;
use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use serde_json::Value;

use crate::k8s::argo::release::{self, Version};

/// How long an answer is kept.
pub const KEPT_FOR: Duration = Duration::from_secs(300);
/// How long to wait for a registry, and the most read of an answer.
const TIMEOUT: Duration = Duration::from_secs(20);
const MAX_BYTES: usize = 8 * 1024 * 1024;
/// Pages of tags read, at most.
const MAX_PAGES: usize = 20;

// MARK: HTTP

/// A GET to send.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Request {
    pub url: String,
    pub headers: Vec<(String, String)>,
}

/// What came back.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Response {
    pub status: u16,
    /// Header names in lower case.
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Response {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers.iter().find(|(n, _)| n == name).map(|(_, v)| v.as_str())
    }
}

/// Send `req` over https, redirects not followed.
async fn send(req: Request) -> Result<Response, String> {
    let client = crate::k8s::argo::api::http_client(false).map_err(|e| format!("{e:#}"))?;
    let mut b = http::Request::builder().uri(&req.url).header("user-agent", "recouvrance");
    for (n, v) in &req.headers {
        b = b.header(n, v);
    }
    let host = host_of(&req.url);
    let r = b.body(Full::new(Bytes::new())).map_err(|e| e.to_string())?;
    let resp = client.request(r).await.map_err(|_| format!("can't reach {host}"))?;
    let status = resp.status().as_u16();
    let headers = resp
        .headers()
        .iter()
        .filter_map(|(n, v)| Some((n.as_str().to_lowercase(), v.to_str().ok()?.to_string())))
        .collect();
    let body = http_body_util::Limited::new(resp.into_body(), MAX_BYTES)
        .collect()
        .await
        .map_err(|_| format!("{host}'s answer was cut"))?
        .to_bytes()
        .to_vec();
    Ok(Response { status, headers, body })
}

fn host_of(url: &str) -> &str {
    url.trim_start_matches("https://").split('/').next().unwrap_or(url)
}

// MARK: Credentials

/// A login to a registry, which never prints.
#[derive(Clone, PartialEq, Eq)]
pub struct Creds {
    user: String,
    secret: String,
}

impl std::fmt::Debug for Creds {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[credentials]")
    }
}

impl Creds {
    fn basic(&self) -> String {
        let pair = format!("{}:{}", self.user, self.secret);
        format!("Basic {}", base64::engine::general_purpose::STANDARD.encode(pair))
    }

    /// `text` with the secret hidden.
    fn hide(&self, text: String) -> String {
        if self.secret.is_empty() { text } else { text.replace(&self.secret, "[secret]") }
    }
}

/// Where the credentials of a registry are, in a Docker config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Source {
    Login(Creds),
    /// A credential helper's name (`docker-credential-<name>`).
    Helper(String),
    None,
}

/// The registry and the repository's path of an image repository: Docker Hub's when it names
/// no registry (`redis` is `library/redis` there).
pub fn registry_of(repo: &str) -> (String, String) {
    match repo.split_once('/') {
        Some((host, path)) if host.contains(['.', ':']) || host == "localhost" => {
            let host = if host == "docker.io" || host == "index.docker.io" { "registry-1.docker.io" } else { host };
            (host.to_string(), path.to_string())
        }
        Some(_) => ("registry-1.docker.io".into(), repo.to_string()),
        None => ("registry-1.docker.io".into(), format!("library/{repo}")),
    }
}

/// The keys a Docker config may file the registry `host` under.
fn config_keys(host: &str) -> Vec<String> {
    if host == "registry-1.docker.io" {
        return vec!["https://index.docker.io/v1/".into(), "index.docker.io".into(), "docker.io".into()];
    }
    vec![host.to_string(), format!("https://{host}"), format!("https://{host}/"), format!("http://{host}")]
}

/// Where the credentials of the registry `host` are in the Docker config `config`.
pub fn source(config: &Value, host: &str) -> Source {
    let keys = config_keys(host);
    let auths = config.get("auths").and_then(Value::as_object);
    for key in &keys {
        let Some(entry) = auths.and_then(|a| a.get(key)) else { continue };
        let text = |k: &str| entry.get(k).and_then(Value::as_str).unwrap_or_default().to_string();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(text("auth").trim())
            .ok()
            .and_then(|b| String::from_utf8(b).ok());
        if let Some((user, secret)) = decoded.as_deref().and_then(|d| d.split_once(':')) {
            return Source::Login(Creds { user: user.to_string(), secret: secret.to_string() });
        }
        if !text("username").is_empty() {
            return Source::Login(Creds { user: text("username"), secret: text("password") });
        }
        if !text("identitytoken").is_empty() {
            return Source::Login(Creds { user: "<token>".into(), secret: text("identitytoken") });
        }
    }
    let helpers = config.get("credHelpers").and_then(Value::as_object);
    for key in &keys {
        if let Some(h) = helpers.and_then(|h| h.get(key)).and_then(Value::as_str).filter(|h| !h.is_empty()) {
            return Source::Helper(h.to_string());
        }
    }
    match config.get("credsStore").and_then(Value::as_str).filter(|h| !h.is_empty()) {
        Some(h) => Source::Helper(h.to_string()),
        None => Source::None,
    }
}

/// A credential helper's answer: `{"Username": …, "Secret": …}`.
pub fn helper_answer(out: &[u8]) -> Option<Creds> {
    let v: Value = serde_json::from_slice(out).ok()?;
    let secret = v.get("Secret")?.as_str()?.to_string();
    let user = v.get("Username").and_then(Value::as_str).unwrap_or_default().to_string();
    (!secret.is_empty()).then_some(Creds { user, secret })
}

/// The Docker config, as read now (none: empty).
fn docker_config() -> Value {
    let dir = std::env::var_os("DOCKER_CONFIG")
        .filter(|d| !d.is_empty())
        .map_or_else(|| crate::paths::home().join(".docker"), std::path::PathBuf::from);
    std::fs::read(dir.join("config.json")).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or(Value::Null)
}

/// The credentials of the registry `host`: from the Docker config, a helper run when it says
/// so. None when there are none.
async fn creds(host: &str) -> Option<Creds> {
    let config = tokio::task::spawn_blocking(docker_config).await.unwrap_or(Value::Null);
    match source(&config, host) {
        Source::Login(c) => Some(c),
        Source::None => None,
        Source::Helper(name) => {
            if !name.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.')) {
                return None;
            }
            let mut cmd = crate::process::tokio_command(format!("docker-credential-{name}"));
            cmd.arg("get")
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true);
            let mut child = cmd.spawn().ok()?;
            if let Some(mut stdin) = child.stdin.take() {
                use tokio::io::AsyncWriteExt as _;
                let server = if host == "registry-1.docker.io" { "https://index.docker.io/v1/" } else { host };
                stdin.write_all(server.as_bytes()).await.ok()?;
            }
            let out = tokio::time::timeout(Duration::from_secs(10), child.wait_with_output()).await.ok()?.ok()?;
            out.status.success().then(|| helper_answer(&out.stdout)).flatten()
        }
    }
}

// MARK: Reading

/// A `WWW-Authenticate` challenge: its scheme and parameters (`realm`, `service`, `scope`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Challenge {
    pub scheme: String,
    pub params: HashMap<String, String>,
}

/// The challenge of a `WWW-Authenticate` header: `Bearer realm="…",service="…",scope="…"`.
pub fn challenge(header: &str) -> Option<Challenge> {
    let header = header.trim();
    let (scheme, rest) = header.split_once(' ').unwrap_or((header, ""));
    if scheme.is_empty() {
        return None;
    }
    let mut params = HashMap::new();
    let mut rest = rest.trim();
    while !rest.is_empty() {
        let Some((key, after)) = rest.split_once('=') else { break };
        let key = key.trim().trim_start_matches(',').trim().to_lowercase();
        let after = after.trim_start();
        let (value, next) = if let Some(quoted) = after.strip_prefix('"') {
            match quoted.find('"') {
                Some(end) => (&quoted[..end], &quoted[end + 1..]),
                None => (quoted, ""),
            }
        } else {
            match after.find(',') {
                Some(end) => (&after[..end], &after[end..]),
                None => (after, ""),
            }
        };
        params.insert(key, value.to_string());
        rest = next.trim_start().trim_start_matches(',').trim_start();
    }
    Some(Challenge { scheme: scheme.to_lowercase(), params })
}

/// The tags of a `tags/list` answer (`{"name": …, "tags": […]}`; `null` is none).
pub fn tags_of(body: &[u8]) -> Result<Vec<String>, String> {
    let v: Value = serde_json::from_slice(body).map_err(|_| "the registry's tag list isn't JSON".to_string())?;
    match v.get("tags") {
        Some(Value::Array(tags)) => Ok(tags.iter().filter_map(Value::as_str).map(String::from).collect()),
        Some(Value::Null) | None if v.get("name").is_some() => Ok(Vec::new()),
        _ => Err("the registry's answer has no tag list".into()),
    }
}

/// The next page of a list, from its `Link` header (`</v2/x/tags/list?last=b&n=100>;
/// rel="next"`), on the registry `base` (`https://host`).
pub fn next_page(link: Option<&str>, base: &str) -> Option<String> {
    let link = link?;
    let part = link.split(',').find(|p| p.contains("rel=\"next\"") || p.contains("rel=next"))?;
    let url = part.split('<').nth(1)?.split('>').next()?;
    if url.starts_with('/') {
        Some(format!("{base}{url}"))
    } else {
        url.starts_with(&format!("{base}/")).then(|| url.to_string())
    }
}

fn encode(s: &str) -> String {
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

/// A registry asked with `get` (a GET sent): its address, the repository's path, its
/// credentials, and the authorization it got.
struct Session<'a, F> {
    base: String,
    path: String,
    creds: &'a Creds,
    get: &'a F,
    auth: Option<String>,
}

impl<F, Fut> Session<'_, F>
where
    F: Fn(Request) -> Fut,
    Fut: Future<Output = Result<Response, String>>,
{
    fn request(&self, url: &str, accept: Option<&str>) -> Request {
        let mut headers = Vec::new();
        if let Some(a) = &self.auth {
            headers.push(("authorization".to_string(), a.clone()));
        }
        if let Some(a) = accept {
            headers.push(("accept".to_string(), a.to_string()));
        }
        Request { url: url.to_string(), headers }
    }

    /// GET `url` on the registry, authorizing once when it asks (its token flow).
    async fn get(&mut self, url: &str, accept: Option<&str>) -> Result<Response, String> {
        let first = (self.get)(self.request(url, accept)).await?;
        if first.status != 401 || self.auth.is_some() {
            return Ok(first);
        }
        let host = host_of(&self.base).to_string();
        let ch = first.header("www-authenticate").and_then(challenge);
        self.auth = Some(match ch {
            Some(c) if c.scheme == "bearer" => self.token(&c).await?,
            _ => self.creds.basic(),
        });
        let again = (self.get)(self.request(url, accept)).await?;
        if again.status == 401 || again.status == 403 {
            return Err(format!("{host} refused the credentials of the Docker config"));
        }
        Ok(again)
    }

    /// A pull token from the challenge's realm, asked with the credentials.
    async fn token(&self, c: &Challenge) -> Result<String, String> {
        let host = host_of(&self.base).to_string();
        let realm = c
            .params
            .get("realm")
            .filter(|r| r.starts_with("https://"))
            .ok_or_else(|| format!("{host} asks for a token from a realm that isn't https"))?;
        let mut query = Vec::new();
        if let Some(s) = c.params.get("service") {
            query.push(format!("service={}", encode(s)));
        }
        let scope = c.params.get("scope").cloned().unwrap_or_else(|| format!("repository:{}:pull", self.path));
        query.push(format!("scope={}", encode(&scope)));
        let sep = if realm.contains('?') { '&' } else { '?' };
        let url = format!("{realm}{sep}{}", query.join("&"));
        let req = Request { url, headers: vec![("authorization".into(), self.creds.basic())] };
        let resp = (self.get)(req).await?;
        if resp.status == 401 || resp.status == 403 {
            return Err(format!("{host}'s token service refused the credentials of the Docker config"));
        }
        if resp.status != 200 {
            return Err(format!("{host}'s token service answered {}", resp.status));
        }
        let v: Value = serde_json::from_slice(&resp.body).map_err(|_| format!("{host}'s token isn't JSON"))?;
        let token = v.get("token").or_else(|| v.get("access_token")).and_then(Value::as_str).unwrap_or_default();
        if token.is_empty() {
            return Err(format!("{host} gave no token"));
        }
        Ok(format!("Bearer {token}"))
    }

    /// Every tag of the repository, page after page.
    async fn tags(&mut self) -> Result<Vec<String>, String> {
        let host = host_of(&self.base).to_string();
        let mut url = format!("{}/v2/{}/tags/list?n=1000", self.base, self.path);
        let mut out = Vec::new();
        for _ in 0..MAX_PAGES {
            let resp = self.get(&url, Some("application/json")).await?;
            match resp.status {
                200 => {}
                404 => return Err(format!("{host} has no repository {}", self.path)),
                s => return Err(format!("{host} answered {s}")),
            }
            out.extend(tags_of(&resp.body)?);
            match next_page(resp.header("link"), &self.base) {
                Some(next) => url = next,
                None => break,
            }
        }
        Ok(out)
    }

    /// A manifest of the repository (by a tag or a digest), as JSON.
    async fn manifest(&mut self, reference: &str) -> Option<Value> {
        const ACCEPT: &str = "application/vnd.oci.image.index.v1+json, application/vnd.oci.image.manifest.v1+json, \
             application/vnd.docker.distribution.manifest.list.v2+json, \
             application/vnd.docker.distribution.manifest.v2+json";
        let url = format!("{}/v2/{}/manifests/{}", self.base, self.path, encode(reference));
        let r = self.get(&url, Some(ACCEPT)).await.ok().filter(|r| r.status == 200)?;
        serde_json::from_slice::<Value>(&r.body).ok()
    }

    /// When the image `tag` was built: its config's `created` (the first image of an index).
    async fn created(&mut self, tag: &str) -> Option<jiff::Timestamp> {
        let mut m = self.manifest(tag).await?;
        if let Some(first) = m.pointer("/manifests/0/digest").and_then(Value::as_str).map(String::from) {
            m = self.manifest(&first).await?;
        }
        let digest = m.pointer("/config/digest")?.as_str()?.to_string();
        let url = format!("{}/v2/{}/blobs/{}", self.base, self.path, encode(&digest));
        let mut r = self.get(&url, None).await.ok()?;
        // A blob is often served from storage elsewhere: one redirect, over https, without the
        // registry's authorization.
        if matches!(r.status, 301 | 302 | 303 | 307 | 308) {
            let to = r.header("location")?.to_string();
            if !to.starts_with("https://") {
                return None;
            }
            r = (self.get)(Request { url: to, headers: Vec::new() }).await.ok()?;
        }
        let v: Value = serde_json::from_slice(&r.body).ok().filter(|_| r.status == 200)?;
        v.get("created")?.as_str()?.parse().ok()
    }
}

/// What the registry says of a deployed tag.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// The Docker config has no credentials for its registry: not checked.
    NoCredentials(String),
    /// No release tag is newer.
    Latest,
    /// A newer release tag, its version, and when its image was built when known.
    Newer { tag: String, version: String, published: Option<jiff::Timestamp> },
    /// The tag deployed isn't a version: nothing to compare.
    NotAVersion,
    /// Reading the registry failed, and no answer was read before.
    Failed(String),
}

/// What is known of a deployed tag: the registry's answer, and why the last read failed when
/// the answer is an older one.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub status: Status,
    pub stale: Option<String>,
    /// When it was read.
    pub at: jiff::Timestamp,
}

/// Check `repo` (an image repository) at the tag `deployed`, with the credentials `creds`, by
/// `get`: its tags read, and the newest release's date when it is newer.
pub async fn check_with<F, Fut>(repo: &str, deployed: &str, creds: &Creds, get: &F) -> Result<Status, String>
where
    F: Fn(Request) -> Fut,
    Fut: Future<Output = Result<Response, String>>,
{
    let version: Version = release::version_of(deployed);
    if !version.numeric() {
        return Ok(Status::NotAVersion);
    }
    let (host, path) = registry_of(repo);
    let mut s = Session { base: format!("https://{host}"), path, creds, get, auth: None };
    let tags = s.tags().await.map_err(|e| creds.hide(e))?;
    Ok(match release::newest(tags.iter().map(String::as_str), &version) {
        None => Status::Latest,
        Some((tag, v)) => {
            let published = s.created(&tag).await;
            Status::Newer { tag, version: v.text, published }
        }
    })
}

type Cache = HashMap<(String, String), (Check, Instant)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);
/// Answers kept, at most.
const KEPT: usize = 2000;

/// What is known of `repo` at `deployed`, and whether it is to be read again (none yet, or
/// older than [`KEPT_FOR`]).
pub fn known(repo: &str, deployed: &str) -> (Option<Check>, bool) {
    let cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    match cache.get(&(repo.to_string(), deployed.to_string())) {
        Some((c, at)) => (Some(c.clone()), at.elapsed() >= KEPT_FOR),
        None => (None, true),
    }
}

/// Keep what reading `key` gave: an answer replaces the one kept; a failure keeps the last
/// answer, saying why.
fn keep(key: (String, String), read: Result<Status, String>) {
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    if cache.len() >= KEPT && !cache.contains_key(&key) {
        cache.clear();
    }
    let now = jiff::Timestamp::now();
    let check = match (read, cache.get(&key)) {
        (Ok(status), _) => Check { status, stale: None, at: now },
        (Err(why), Some((last, _))) if !matches!(last.status, Status::Failed(_)) => {
            Check { status: last.status.clone(), stale: Some(why), at: last.at }
        }
        (Err(why), _) => Check { status: Status::Failed(why), stale: None, at: now },
    };
    cache.insert(key, (check, Instant::now()));
}

/// Keep `status` as what the registry said of `repo` at `deployed`: the developer tools'
/// made-up example.
#[cfg(feature = "dev")]
pub fn pretend(repo: &str, deployed: &str, status: Status) {
    keep((repo.to_string(), deployed.to_string()), Ok(status));
}

/// Check each of `images` (repository, tag deployed) not checked within [`KEPT_FOR`], one
/// after the other; each answer is kept (see [`known`]).
pub async fn check(images: Vec<(String, String)>) {
    let work = async move {
        let get = |r: Request| send(r);
        for (repo, tag) in images {
            if !known(&repo, &tag).1 {
                continue;
            }
            let (host, _) = registry_of(&repo);
            let read = match creds(&host).await {
                None => Ok(Status::NoCredentials(host)),
                Some(c) => tokio::time::timeout(TIMEOUT * 3, check_with(&repo, &tag, &c, &get))
                    .await
                    .unwrap_or_else(|_| Err(format!("{host} did not answer within {} s", (TIMEOUT * 3).as_secs())))
                    .map_err(|e| c.hide(e)),
            };
            keep((repo, tag), read);
        }
        Ok::<(), anyhow::Error>(())
    };
    let _ = crate::k8s::run_abortable(work).await;
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;

    use serde_json::json;

    use super::*;

    fn creds() -> Creds {
        Creds { user: "ann".into(), secret: "s3cret".into() }
    }

    #[test]
    fn credentials_come_from_the_docker_config() {
        let auth = base64::engine::general_purpose::STANDARD.encode("ann:s3cret");
        let config = json!({
            "auths": {"registry.example.com": {"auth": auth}, "https://index.docker.io/v1/": {"identitytoken": "tok"}},
            "credHelpers": {"gcr.example.com": "gcloud"},
            "credsStore": "desktop",
        });
        assert_eq!(source(&config, "registry.example.com"), Source::Login(creds()));
        assert!(matches!(source(&config, "registry-1.docker.io"), Source::Login(_)));
        assert_eq!(source(&config, "gcr.example.com"), Source::Helper("gcloud".into()));
        assert_eq!(source(&config, "other.example.com"), Source::Helper("desktop".into()));
        assert_eq!(source(&json!({}), "registry.example.com"), Source::None);
        assert_eq!(helper_answer(br#"{"ServerURL":"x","Username":"ann","Secret":"s3cret"}"#), Some(creds()));
        assert_eq!(helper_answer(b"credentials not found"), None);
        assert_eq!(format!("{:?}", creds()), "[credentials]");
    }

    #[test]
    fn an_image_names_its_registry() {
        assert_eq!(registry_of("registry.example.com/shop/api"), ("registry.example.com".into(), "shop/api".into()));
        assert_eq!(registry_of("localhost:5000/api"), ("localhost:5000".into(), "api".into()));
        assert_eq!(registry_of("redis"), ("registry-1.docker.io".into(), "library/redis".into()));
        assert_eq!(registry_of("team/api"), ("registry-1.docker.io".into(), "team/api".into()));
        assert_eq!(registry_of("docker.io/team/api"), ("registry-1.docker.io".into(), "team/api".into()));
    }

    #[test]
    fn a_challenge_and_a_page_link_read() {
        let c = challenge(r#"Bearer realm="https://auth.example.com/token",service="registry.example.com",scope="repository:shop/api:pull""#).unwrap();
        assert_eq!(c.scheme, "bearer");
        assert_eq!(c.params["realm"], "https://auth.example.com/token");
        assert_eq!(c.params["scope"], "repository:shop/api:pull");
        assert_eq!(challenge(r#"Basic realm="Registry""#).unwrap().scheme, "basic");
        let base = "https://registry.example.com";
        assert_eq!(
            next_page(Some(r#"</v2/shop/api/tags/list?last=b&n=2>; rel="next""#), base).as_deref(),
            Some("https://registry.example.com/v2/shop/api/tags/list?last=b&n=2")
        );
        assert_eq!(next_page(Some(r#"<https://evil.example.com/x>; rel="next""#), base), None, "another host");
        assert_eq!(next_page(None, base), None);
        assert_eq!(tags_of(br#"{"name":"shop/api","tags":["a","b"]}"#).unwrap(), ["a", "b"]);
        assert_eq!(tags_of(br#"{"name":"shop/api","tags":null}"#).unwrap(), Vec::<String>::new());
        assert!(tags_of(b"<html>").is_err());
    }

    /// A registry that asks for a token, lists its tags on two pages, and serves the newest
    /// image's config through a redirect.
    fn fake(req: Request, seen: &RefCell<Vec<Request>>) -> Result<Response, String> {
        seen.borrow_mut().push(req.clone());
        let auth = req.headers.iter().find(|(n, _)| n == "authorization").map(|(_, v)| v.as_str());
        let ok = |body: Value| Response { status: 200, headers: Vec::new(), body: body.to_string().into_bytes() };
        let url = req.url.as_str();
        if url.starts_with("https://auth.example.com/token") {
            return Ok(if auth == Some(creds().basic().as_str()) {
                ok(json!({"token": "pull-token"}))
            } else {
                Response { status: 401, ..Response::default() }
            });
        }
        if url.starts_with("https://storage.example.com/") {
            assert_eq!(auth, None, "the registry's token isn't sent elsewhere");
            return Ok(ok(json!({"created": "2026-11-27T10:00:00Z"})));
        }
        if auth != Some("Bearer pull-token") {
            return Ok(Response {
                status: 401,
                headers: vec![(
                    "www-authenticate".into(),
                    r#"Bearer realm="https://auth.example.com/token",service="registry.example.com""#.into(),
                )],
                body: Vec::new(),
            });
        }
        let path = url.trim_start_matches("https://registry.example.com");
        Ok(match path {
            "/v2/shop/api/tags/list?n=1000" => Response {
                headers: vec![("link".into(), r#"</v2/shop/api/tags/list?last=b&n=1000>; rel="next""#.into())],
                ..ok(json!({"name": "shop/api", "tags": ["v26-11-9-aaaaaaa", "v26-11-22-bbbbbbb", "latest"]}))
            },
            "/v2/shop/api/tags/list?last=b&n=1000" => {
                ok(json!({"name": "shop/api", "tags": ["v26-11-23-ccccccc", "v26-11-23-ccccccc-chrome"]}))
            }
            "/v2/shop/api/manifests/v26-11-23-ccccccc" => ok(json!({"manifests": [{"digest": "sha256:index0"}]})),
            "/v2/shop/api/manifests/sha256%3Aindex0" => ok(json!({"config": {"digest": "sha256:cfg"}})),
            "/v2/shop/api/blobs/sha256%3Acfg" => Response {
                status: 307,
                headers: vec![("location".into(), "https://storage.example.com/blob".into())],
                body: Vec::new(),
            },
            _ => Response { status: 404, ..Response::default() },
        })
    }

    #[test]
    fn the_newest_release_tag_is_read_through_the_token_flow() {
        let seen = RefCell::new(Vec::new());
        let get = |r: Request| {
            let out = fake(r, &seen);
            async move { out }
        };
        let status = futures::executor::block_on(check_with(
            "registry.example.com/shop/api",
            "v26-11-22-bbbbbbb",
            &creds(),
            &get,
        ))
        .unwrap();
        assert_eq!(
            status,
            Status::Newer {
                tag: "v26-11-23-ccccccc".into(),
                version: "v26.11.23".into(),
                published: "2026-11-27T10:00:00Z".parse().ok(),
            }
        );
        let token = seen.borrow().iter().find(|r| r.url.starts_with("https://auth.example.com")).cloned().unwrap();
        assert!(token.url.contains("scope=repository%3Ashop%2Fapi%3Apull"), "{}", token.url);
        // Up to date, and a tag that isn't a version.
        let latest = futures::executor::block_on(check_with(
            "registry.example.com/shop/api",
            "v26-11-23-ccccccc",
            &creds(),
            &get,
        ));
        assert_eq!(latest, Ok(Status::Latest));
        let branch =
            futures::executor::block_on(check_with("registry.example.com/shop/api", "main-abcdef1", &creds(), &get));
        assert_eq!(branch, Ok(Status::NotAVersion));
    }

    #[test]
    fn refused_credentials_are_said_without_the_secret() {
        let wrong = Creds { user: "ann".into(), secret: "nope".into() };
        let get = |r: Request| {
            let out = fake(r, &RefCell::new(Vec::new()));
            async move { out }
        };
        let e = futures::executor::block_on(check_with("registry.example.com/shop/api", "v26-11-22-b", &wrong, &get))
            .unwrap_err();
        assert!(e.contains("refused") && !e.contains("nope"), "{e}");
    }

    #[test]
    fn a_failed_read_keeps_the_last_answer() {
        let key = ("registry.example.com/test/keep".to_string(), "1.0.0".to_string());
        keep(key.clone(), Ok(Status::Latest));
        keep(key.clone(), Err("can't reach registry.example.com".into()));
        let (c, _) = known(&key.0, &key.1);
        let c = c.unwrap();
        assert_eq!(c.status, Status::Latest);
        assert_eq!(c.stale.as_deref(), Some("can't reach registry.example.com"));
        let fresh = ("registry.example.com/test/never".to_string(), "1.0.0".to_string());
        keep(fresh.clone(), Err("down".into()));
        assert_eq!(known(&fresh.0, &fresh.1).0.unwrap().status, Status::Failed("down".into()));
        assert!(known("registry.example.com/test/unknown", "1").1, "never read: to read");
    }
}
