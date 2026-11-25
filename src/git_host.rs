//! The commit an image was built from, read from its git host: an image tagged
//! `main-<sha>` or `26-8-4-<sha>` holds the commit's sha, and the host says its title, author,
//! date and page.
//!
//! The hosts and their tokens are those the user already has: the GitLab CLI's (`glab`,
//! `config.yml`: each host's `token`, `api_host`, `api_protocol` and
//! `container_registry_domains`), the GitHub CLI's (`gh`, `hosts.yml`: `oauth_token`), and the
//! `GITLAB_TOKEN` (for glab's default host) and `GH_TOKEN` / `GITHUB_TOKEN` (for github.com)
//! variables. They are only read. A token kept in the system keyring is not: with none, nothing
//! is shown. Only https is used, redirects are not followed, and a token never shows in an
//! error.
//!
//! An image's repository names its project: `registry.example.com/shop/api` is `shop/api` on
//! a GitLab (its registry under the project, so `shop` is tried too); `ghcr.io/owner/repo` is
//! `owner/repo` on GitHub. A GitLab whose registry domains name the image's registry is
//! asked first, then the other GitLab hosts. Each answer is kept for the session (a commit
//! doesn't change), a project that doesn't have it too; a failure is asked again after a while.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use bytes::Bytes;
use http_body_util::{BodyExt as _, Full};
use serde::Deserialize;
use serde_json::Value;

/// A commit, as its host says it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Commit {
    pub sha: String,
    /// Its message's first line.
    pub title: String,
    pub author: String,
    pub date: Option<jiff::Timestamp>,
    /// Its page on the host.
    pub url: String,
}

/// What is known of an image's commit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Lookup {
    Found(Commit),
    /// No host has it, or none is set up: nothing to show.
    None,
    /// Asking failed: why (never holding a token).
    Failed(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Forge {
    GitLab,
    GitHub,
}

/// A token, which never prints.
#[derive(Clone, PartialEq, Eq)]
struct Token(String);

impl std::fmt::Debug for Token {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("[token]")
    }
}

/// A git host to ask, and its token.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub forge: Forge,
    /// `gitlab.example.com`.
    pub name: String,
    /// Where its API is: `https://gitlab.example.com/api/v4`, `https://api.github.com`.
    api: String,
    /// The registries whose images are its projects'.
    registries: Vec<String>,
    token: Token,
}

// MARK: Hosts

#[derive(Deserialize, Default)]
struct GlabConfig {
    #[serde(default)]
    host: String,
    #[serde(default)]
    hosts: HashMap<String, GlabHost>,
}

#[derive(Deserialize, Default)]
struct GlabHost {
    #[serde(default)]
    token: Option<String>,
    #[serde(default)]
    api_host: Option<String>,
    #[serde(default)]
    api_protocol: Option<String>,
    #[serde(default)]
    container_registry_domains: Option<String>,
}

#[derive(Deserialize, Default)]
struct GhHost {
    #[serde(default)]
    oauth_token: Option<String>,
}

/// A token as written: none when empty.
fn token(t: Option<&str>) -> Option<Token> {
    t.map(str::trim).filter(|t| !t.is_empty() && *t != "null").map(|t| Token(t.to_string()))
}

/// The GitLab hosts of glab's config `text`, `env_token` (`GITLAB_TOKEN`) standing for its
/// default host's. A host on plain http is left out.
fn glab_hosts(text: &str, env_token: Option<&str>) -> Vec<Host> {
    let Ok(config) = crate::yaml::from_str::<GlabConfig>(text) else { return Vec::new() };
    let mut names: Vec<&String> = config.hosts.keys().collect();
    names.sort();
    let mut out: Vec<Host> = names
        .into_iter()
        .filter_map(|name| {
            let h = &config.hosts[name];
            if h.api_protocol.as_deref().is_some_and(|p| p != "https") {
                return None;
            }
            let env = (*name == config.host).then_some(env_token).flatten();
            let token = token(env).or_else(|| token(h.token.as_deref()))?;
            let api_host = h.api_host.as_deref().map(str::trim).filter(|a| !a.is_empty()).unwrap_or(name);
            let registries = h
                .container_registry_domains
                .as_deref()
                .unwrap_or_default()
                .split(',')
                .map(|d| d.trim().to_lowercase())
                .filter(|d| !d.is_empty())
                .collect();
            Some(Host {
                forge: Forge::GitLab,
                name: name.clone(),
                api: format!("https://{api_host}/api/v4"),
                registries,
                token,
            })
        })
        .collect();
    // The default host first: most images are its.
    out.sort_by_key(|h| h.name != config.host);
    out
}

/// The GitHub hosts of gh's `hosts.yml` `text`; `env_token` (`GH_TOKEN`, `GITHUB_TOKEN`) for
/// github.com when the file has none.
fn gh_hosts(text: &str, env_token: Option<&str>) -> Vec<Host> {
    let hosts: HashMap<String, GhHost> = crate::yaml::from_str(text).unwrap_or_default();
    let mut out: Vec<Host> = hosts
        .iter()
        .filter_map(|(name, h)| {
            let env = (name == "github.com").then_some(env_token).flatten();
            Some(Host {
                forge: Forge::GitHub,
                name: name.clone(),
                api: github_api(name),
                registries: Vec::new(),
                token: token(h.oauth_token.as_deref()).or_else(|| token(env))?,
            })
        })
        .collect();
    if !out.iter().any(|h| h.name == "github.com")
        && let Some(token) = token(env_token)
    {
        out.push(Host {
            forge: Forge::GitHub,
            name: "github.com".into(),
            api: github_api("github.com"),
            registries: Vec::new(),
            token,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn github_api(name: &str) -> String {
    if name == "github.com" { "https://api.github.com".into() } else { format!("https://{name}/api/v3") }
}

/// A CLI's config file: under `$<var>` when set, else `$XDG_CONFIG_HOME/<dir>/<file>`.
fn config_file(var: &str, dir: &str, file: &str) -> PathBuf {
    if let Some(d) = std::env::var_os(var).filter(|d| !d.is_empty()) {
        return PathBuf::from(d).join(file);
    }
    std::env::var_os("XDG_CONFIG_HOME")
        .filter(|d| !d.is_empty())
        .map_or_else(|| crate::paths::home().join(".config"), PathBuf::from)
        .join(dir)
        .join(file)
}

/// The git hosts set up on this machine (see the module's doc).
pub fn hosts() -> Vec<Host> {
    let env = |k: &str| std::env::var(k).ok().filter(|v| !v.trim().is_empty());
    let read = |p: PathBuf| std::fs::read_to_string(p).unwrap_or_default();
    let mut out =
        glab_hosts(&read(config_file("GLAB_CONFIG_DIR", "glab-cli", "config.yml")), env("GITLAB_TOKEN").as_deref());
    out.extend(gh_hosts(
        &read(config_file("GH_CONFIG_DIR", "gh", "hosts.yml")),
        env("GH_TOKEN").or_else(|| env("GITHUB_TOKEN")).as_deref(),
    ));
    out
}

// MARK: Images

/// The commit sha a tag holds: its last part (after `-`, `_` or `.`), 7 to 40 hex digits
/// with a letter in them (so a build number isn't one).
pub fn sha_of(tag: &str) -> Option<&str> {
    let last = tag.rsplit(['-', '_', '.']).next()?;
    let hex = (7..=40).contains(&last.len()) && last.bytes().all(|b| b.is_ascii_hexdigit());
    let lower = last.bytes().all(|b| !b.is_ascii_uppercase());
    (hex && lower && last.bytes().any(|b| b.is_ascii_alphabetic())).then_some(last)
}

/// Where an image's project may be, in the order to ask: (host index, project path).
pub fn candidates(repo: &str, hosts: &[Host]) -> Vec<(usize, String)> {
    let Some((registry, path)) = repo.split_once('/') else { return Vec::new() };
    // `nginx` or `library/nginx` on Docker Hub: no registry named, not the product's.
    if !registry.contains(['.', ':']) || registry == "docker.io" {
        return Vec::new();
    }
    let registry = registry.to_lowercase();
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    let mut out = Vec::new();
    // GitLab: the project, else the project above (an image under a project's registry).
    let gitlab = |i: usize, out: &mut Vec<(usize, String)>| {
        for n in (2..=parts.len()).rev().take(2) {
            out.push((i, parts[..n].join("/")));
        }
    };
    let owns = |h: &Host| {
        h.registries.contains(&registry)
            || registry == h.name
            || registry.strip_prefix("registry.").is_some_and(|r| r == h.name)
    };
    for (i, _) in hosts.iter().enumerate().filter(|(_, h)| h.forge == Forge::GitLab && owns(h)) {
        gitlab(i, &mut out);
    }
    for (i, _) in hosts.iter().enumerate().filter(|(_, h)| h.forge == Forge::GitLab && !owns(h)) {
        gitlab(i, &mut out);
    }
    let github = registry == "ghcr.io" || registry.starts_with("docker.pkg.");
    if github && parts.len() >= 2 {
        let host = if registry == "ghcr.io" { "github.com" } else { registry.trim_start_matches("docker.pkg.") };
        if let Some(i) = hosts.iter().position(|h| h.forge == Forge::GitHub && h.name == host) {
            out.push((i, format!("{}/{}", parts[0], parts[1])));
        }
    }
    out
}

// MARK: Reading

/// What a host says of a project's commit: it, none (the project or the commit isn't there,
/// or isn't readable with this token), or why it couldn't be asked.
fn parse_commit(forge: Forge, v: &Value) -> Option<Commit> {
    let text = |p: &str| v.pointer(p).and_then(Value::as_str).unwrap_or_default().to_string();
    let (sha, message, author, date, url) = match forge {
        Forge::GitLab => (text("/id"), text("/title"), text("/author_name"), text("/committed_date"), text("/web_url")),
        Forge::GitHub => (
            text("/sha"),
            text("/commit/message"),
            text("/commit/author/name"),
            text("/commit/author/date"),
            text("/html_url"),
        ),
    };
    if sha.is_empty() {
        return None;
    }
    let url = if url.starts_with("https://") { url } else { String::new() };
    Some(Commit {
        sha,
        title: message.lines().next().unwrap_or_default().trim().to_string(),
        author,
        date: date.parse::<jiff::Timestamp>().ok(),
        url,
    })
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

/// The address of `project`'s commit `sha` on `host`'s API.
fn commit_url(host: &Host, project: &str, sha: &str) -> String {
    match host.forge {
        Forge::GitLab => format!("{}/projects/{}/repository/commits/{}", host.api, encode(project), encode(sha)),
        Forge::GitHub => {
            let (owner, repo) = project.split_once('/').unwrap_or((project, ""));
            format!("{}/repos/{}/{}/commits/{}", host.api, encode(owner), encode(repo), encode(sha))
        }
    }
}

/// How long to wait for a host, and the most read of an answer.
const TIMEOUT: Duration = Duration::from_secs(15);
const MAX_BYTES: usize = 1024 * 1024;

/// Ask `host` for `project`'s commit `sha`: Ok(None) when it hasn't it (404, or no access).
async fn ask(host: &Host, project: &str, sha: &str) -> Result<Option<Commit>, String> {
    let fetch = async {
        let client = crate::k8s::argo::api::http_client(false).map_err(|e| format!("{e:#}"))?;
        let mut req = http::Request::builder()
            .uri(commit_url(host, project, sha))
            .header("user-agent", "recouvrance")
            .header("authorization", format!("Bearer {}", host.token.0));
        req = match host.forge {
            Forge::GitLab => req.header("accept", "application/json"),
            Forge::GitHub => req.header("accept", "application/vnd.github+json"),
        };
        let req = req.body(Full::new(Bytes::new())).map_err(|e| e.to_string())?;
        let resp = client.request(req).await.map_err(|_| format!("can't reach {}", host.name))?;
        let status = resp.status();
        if matches!(status.as_u16(), 401 | 403 | 404 | 422) {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(format!("{} answered {status}", host.name));
        }
        let body = http_body_util::Limited::new(resp.into_body(), MAX_BYTES)
            .collect()
            .await
            .map_err(|_| format!("{}'s answer was cut", host.name))?;
        let v: Value =
            serde_json::from_slice(&body.to_bytes()).map_err(|_| format!("{}'s answer isn't JSON", host.name))?;
        Ok(parse_commit(host.forge, &v))
    };
    let out = tokio::time::timeout(TIMEOUT, fetch)
        .await
        .unwrap_or_else(|_| Err(format!("{} did not answer within {} s", host.name, TIMEOUT.as_secs())));
    // Whatever failed, the token is never said.
    out.map_err(|e| e.replace(&host.token.0, "[token]"))
}

/// A failure is asked again after this.
const RETRY: Duration = Duration::from_secs(300);
/// Answers kept, at most.
const KEPT: usize = 5000;

type Cache = HashMap<(String, String), (Lookup, Instant)>;
static CACHE: LazyLock<Mutex<Cache>> = LazyLock::new(Default::default);

/// What is known now of the commit of `repo` at `tag`, without asking: None when it has to be
/// asked (`read`).
pub fn known(repo: &str, tag: &str) -> Option<Lookup> {
    let Some(sha) = sha_of(tag) else { return Some(Lookup::None) };
    let cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    let (lookup, at) = cache.get(&(repo.to_string(), sha.to_string()))?;
    (!matches!(lookup, Lookup::Failed(_)) || at.elapsed() < RETRY).then(|| lookup.clone())
}

/// Keep `commit` as that of `repo` at `tag`, as if its host had said it: the developer tools'
/// made-up example.
#[cfg(feature = "dev")]
pub fn pretend(repo: &str, tag: &str, commit: Commit) {
    let Some(sha) = sha_of(tag) else { return };
    let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
    cache.insert((repo.to_string(), sha.to_string()), (Lookup::Found(commit), Instant::now()));
}

/// The commits of `images` (repository, tag), read from their hosts where not known yet (one
/// image at a time per host's answer, the hosts asked in order). Each answer is kept.
pub async fn read(images: Vec<(String, String)>) -> HashMap<(String, String), Lookup> {
    let work = async move {
        let hosts = tokio::task::spawn_blocking(hosts).await.unwrap_or_default();
        let mut out = HashMap::new();
        for (repo, tag) in images {
            if let Some(k) = known(&repo, &tag) {
                out.insert((repo, tag), k);
                continue;
            }
            let Some(sha) = sha_of(&tag).map(String::from) else { continue };
            let mut lookup = Lookup::None;
            for (h, project) in candidates(&repo, &hosts) {
                match ask(&hosts[h], &project, &sha).await {
                    Ok(Some(c)) => {
                        lookup = Lookup::Found(c);
                        break;
                    }
                    Ok(None) => {}
                    Err(e) => lookup = Lookup::Failed(e),
                }
            }
            let mut cache = CACHE.lock().unwrap_or_else(PoisonError::into_inner);
            if cache.len() >= KEPT {
                cache.clear();
            }
            cache.insert((repo.clone(), sha), (lookup.clone(), Instant::now()));
            out.insert((repo, tag), lookup);
        }
        Ok(out)
    };
    crate::k8s::run_abortable(work).await.unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gitlab(name: &str, registries: &[&str]) -> Host {
        Host {
            forge: Forge::GitLab,
            name: name.into(),
            api: format!("https://{name}/api/v4"),
            registries: registries.iter().map(ToString::to_string).collect(),
            token: Token("t".into()),
        }
    }

    #[test]
    fn a_tag_holds_a_sha_only_as_its_last_hex_part() {
        let sha = "77b324b4a0c4f1e2d3b4a5968778695a4b3c2d1e";
        assert_eq!(sha_of(&format!("main-{sha}")), Some(sha));
        assert_eq!(sha_of(&format!("26-8-4-{sha}")), Some(sha));
        assert_eq!(sha_of("main-77b324b"), Some("77b324b"));
        assert_eq!(sha_of("1.4.2"), None);
        assert_eq!(sha_of("build-1234567"), None, "digits only: a build number");
        assert_eq!(sha_of("latest"), None);
        assert_eq!(sha_of("v1.2.3-ABCDEF1"), None);
        assert_eq!(sha_of(&format!("main-{sha}0")), None, "longer than a sha");
    }

    #[test]
    fn hosts_come_from_the_clis_config_over_https_only() {
        let glab = "host: gitlab.example.com\nhosts:\n  gitlab.example.com:\n    token: abc\n    api_protocol: https\n    \
                    container_registry_domains: gitlab.example.com,registry.example.com\n  plain.example.com:\n    \
                    token: def\n    api_protocol: http\n  gitlab.com:\n    token: ''\n";
        let h = glab_hosts(glab, None);
        assert_eq!(h.len(), 1, "plain http and an empty token are left out");
        assert_eq!(h[0].api, "https://gitlab.example.com/api/v4");
        assert_eq!(h[0].registries, ["gitlab.example.com", "registry.example.com"]);
        assert_eq!(glab_hosts(glab, Some("env"))[0].token, Token("env".into()));
        assert!(glab_hosts("not: [yaml", None).is_empty());
        let gh = gh_hosts("github.com:\n  user: me\n", None);
        assert!(gh.is_empty(), "a token in the keyring isn't read");
        let gh = gh_hosts("github.com:\n  user: me\n", Some("x"));
        assert_eq!((gh[0].name.as_str(), gh[0].api.as_str()), ("github.com", "https://api.github.com"));
        assert_eq!(gh_hosts("", Some("x")).len(), 1);
        assert_eq!(format!("{:?}", gh[0].token), "[token]");
    }

    #[test]
    fn the_gitlab_of_the_registry_is_asked_first_then_the_project_above() {
        let hosts = [gitlab("gitlab.other.com", &[]), gitlab("gitlab.example.com", &["registry.example.com"])];
        assert_eq!(
            candidates("registry.example.com/shop/back/api", &hosts),
            [
                (1, "shop/back/api".to_string()),
                (1, "shop/back".to_string()),
                (0, "shop/back/api".to_string()),
                (0, "shop/back".to_string())
            ]
        );
        assert!(candidates("redis", &hosts).is_empty());
        assert!(candidates("library/redis", &hosts).is_empty());
        assert!(candidates("docker.io/library/redis", &hosts).is_empty());
        let gh = Host { forge: Forge::GitHub, name: "github.com".into(), ..gitlab("github.com", &[]) };
        assert_eq!(candidates("ghcr.io/owner/repo/sub", &[gh]), [(0, "owner/repo".to_string())]);
    }

    #[test]
    fn a_commit_reads_from_either_host() {
        let lab = serde_json::json!({"id": "abc1234", "title": "fix: x", "author_name": "Ann", "committed_date": "2026-11-20T10:00:00+01:00", "web_url": "https://gitlab.example.com/shop/api/-/commit/abc1234"});
        let c = parse_commit(Forge::GitLab, &lab).unwrap();
        assert_eq!((c.title.as_str(), c.author.as_str()), ("fix: x", "Ann"));
        assert!(c.date.is_some());
        let hub = serde_json::json!({"sha": "abc", "html_url": "javascript:alert(1)", "commit": {"message": "feat: y\n\nbody", "author": {"name": "Bo", "date": "2026-11-20T10:00:00Z"}}});
        let c = parse_commit(Forge::GitHub, &hub).unwrap();
        assert_eq!((c.title.as_str(), c.url.as_str()), ("feat: y", ""), "only an https link is kept");
        assert_eq!(parse_commit(Forge::GitHub, &serde_json::json!({})), None);
        assert_eq!(
            commit_url(&gitlab("gitlab.example.com", &[]), "shop/api", "abc"),
            "https://gitlab.example.com/api/v4/projects/shop%2Fapi/repository/commits/abc"
        );
    }
}
