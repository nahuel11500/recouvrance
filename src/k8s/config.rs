//! Kubeconfig files and contexts: merging, watching, connecting, private copies for tools.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock};

use anyhow::{Context as _, Result, anyhow};
use kube::config::{KubeConfigOptions, Kubeconfig};
use kube::{Client, Config};

use super::{added_dir, folder_files, gate, login, meter, retry, run};
use crate::paths::tilde;
use crate::private_files::{TempFile, save_new};

#[derive(Clone)]
pub struct Contexts {
    pub names: Vec<String>,
    pub current: Option<String>,
    /// Contexts whose name or user suggests production or admin rights.
    pub risky: Vec<String>,
    /// What each context points to (see `fingerprint`), to notice it changed.
    pub fingerprints: HashMap<String, u64>,
    /// False while a file is read without a context name it had and may be half-written:
    /// the name stays with it until the file has been seen unchanged for `SETTLE`.
    pub settled: bool,
}

/// Every kubeconfig file in use: KUBECONFIG (or ~/.kube/config), the extra files
/// from settings, each file in the watched folders and the kubeconfigs pasted in
/// Settings. Existing files only, no duplicates.
pub fn kubeconfig_files() -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = match std::env::var_os("KUBECONFIG").filter(|v| !v.is_empty()) {
        Some(v) => std::env::split_paths(&v).collect(),
        None => {
            vec![crate::paths::default_kubeconfig()]
        }
    };
    let st = crate::settings::get();
    files.extend(st.kubeconfigs.iter().map(|p| crate::paths::expand(p)));
    for dir in &st.kubeconfig_folders {
        files.extend(folder_files(&crate::paths::expand(dir)));
    }
    // Kubeconfigs pasted in Settings.
    files.extend(folder_files(&added_dir()));
    let mut seen = std::collections::HashSet::new();
    files.retain(|p| p.is_file() && seen.insert(p.canonicalize().unwrap_or_else(|_| p.clone())));
    files
}

/// When and how big each kubeconfig file is, to notice changes.
pub fn kubeconfig_signature() -> Vec<(std::path::PathBuf, Option<std::time::SystemTime>, u64)> {
    kubeconfig_files()
        .into_iter()
        .map(|p| {
            let m = std::fs::metadata(&p).ok();
            let (t, len) = (m.as_ref().and_then(|m| m.modified().ok()), m.map_or(0, |m| m.len()));
            (p, t, len)
        })
        .collect()
}

/// Add `more` to `kc`. Names already taken (two files with a context, cluster or
/// user of the same name) get `@<file name>` so both stay usable, then `-2`, `-3`…
/// if that is taken too.
pub(super) fn merge_unique(kc: &mut Kubeconfig, mut more: Kubeconfig, file: &str) {
    // Every name in use, from both sides: a new name must not land on any of them.
    let mut taken_clusters: std::collections::HashSet<String> =
        kc.clusters.iter().chain(&more.clusters).map(|c| c.name.clone()).collect();
    let mut taken_users: std::collections::HashSet<String> =
        kc.auth_infos.iter().chain(&more.auth_infos).map(|c| c.name.clone()).collect();
    let mut taken_contexts: std::collections::HashSet<String> =
        kc.contexts.iter().chain(&more.contexts).map(|c| c.name.clone()).collect();
    for c in &mut more.clusters {
        if kc.clusters.iter().any(|k| k.name == c.name) {
            let old = c.name.clone();
            c.name = free_name(&old, file, &mut taken_clusters);
            for ctx in more.contexts.iter_mut().filter_map(|x| x.context.as_mut()) {
                if ctx.cluster == old {
                    ctx.cluster.clone_from(&c.name);
                }
            }
        }
    }
    for u in &mut more.auth_infos {
        if kc.auth_infos.iter().any(|k| k.name == u.name) {
            let old = u.name.clone();
            u.name = free_name(&old, file, &mut taken_users);
            for ctx in more.contexts.iter_mut().filter_map(|x| x.context.as_mut()) {
                if ctx.user.as_deref() == Some(old.as_str()) {
                    ctx.user = Some(u.name.clone());
                }
            }
        }
    }
    for c in &mut more.contexts {
        if kc.contexts.iter().any(|k| k.name == c.name) {
            let old = c.name.clone();
            c.name = free_name(&old, file, &mut taken_contexts);
            // The file's current context follows its rename.
            if more.current_context.as_deref() == Some(old.as_str()) {
                more.current_context = Some(c.name.clone());
            }
        }
    }
    kc.clusters.extend(more.clusters);
    kc.auth_infos.extend(more.auth_infos);
    kc.contexts.extend(more.contexts);
    if kc.current_context.is_none() {
        kc.current_context = more.current_context;
    }
}

/// `name@file`, or `name@file-2`, `-3`… when that is taken; the result is marked taken.
fn free_name(name: &str, file: &str, taken: &mut std::collections::HashSet<String>) -> String {
    let mut new = format!("{name}@{file}");
    let mut i = 2;
    while taken.contains(&new) {
        new = format!("{name}@{file}-{i}");
        i += 1;
    }
    taken.insert(new.clone());
    new
}

/// All kubeconfig files merged (see `kubeconfig_files`); unreadable files are skipped.
/// A context name several files have stays with the file that had it first (kept in
/// settings), whatever order the files come in: the others get `name@file`.
pub fn kubeconfig() -> Result<Kubeconfig> {
    read_kubeconfig().map(|(kc, _)| kc)
}

/// When a file was last written, and its size.
type Stamp = (Option<std::time::SystemTime>, u64);

/// How long a file must stay unchanged before it gives up a context name: `kubectl config
/// use-context` truncates the file then writes it, so a read in between finds fewer
/// contexts, or none.
const SETTLE: std::time::Duration = std::time::Duration::from_secs(2);

/// Whether a file read with stamps `before` and `after` is settled: unchanged during the
/// read, and since `seen` (each file's stamp and when it was first seen) first saw it at
/// least `SETTLE` ago. A time, not a count of reads: several callers read the files, so
/// two reads can come during the same rewrite. `seen` is brought up to date.
fn settle(
    seen: &mut HashMap<String, (Stamp, std::time::Instant)>,
    key: &str,
    before: Option<Stamp>,
    after: Option<Stamp>,
    now: std::time::Instant,
) -> bool {
    let Some(after) = after else {
        seen.remove(key);
        return false;
    };
    let first = seen.entry(key.to_string()).or_insert((after, now));
    if first.0 != after {
        *first = (after, now);
    }
    before == Some(after) && now.saturating_duration_since(first.1) >= SETTLE
}

/// Like `kubeconfig`, and whether it is settled (see `Contexts::settled`).
fn read_kubeconfig() -> Result<(Kubeconfig, bool)> {
    static SEEN: LazyLock<std::sync::Mutex<HashMap<String, (Stamp, std::time::Instant)>>> =
        LazyLock::new(Default::default);
    let stamp = |path: &std::path::Path| -> Option<Stamp> {
        let m = std::fs::metadata(path).ok()?;
        Some((m.modified().ok(), m.len()))
    };
    let mut files = Vec::new();
    let mut settled = std::collections::HashSet::new();
    let mut last_err = None;
    for path in kubeconfig_files() {
        let stem = path.file_stem().map_or_else(|| "file".into(), |s| s.to_string_lossy().to_string());
        let key = tilde(&path.canonicalize().unwrap_or_else(|_| path.clone()));
        let before = stamp(&path);
        let kc = read_kubeconfig_file(&path);
        let after = stamp(&path);
        let mut seen = SEEN.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if settle(&mut seen, &key, before, after, std::time::Instant::now()) {
            settled.insert(key.clone());
        }
        drop(seen);
        match kc {
            Ok(kc) => files.push((stem, key, Some(kc))),
            Err(e) => {
                last_err = Some(e);
                files.push((stem, key, None));
            }
        }
    }
    let names: Vec<(String, Option<Vec<String>>)> = files
        .iter()
        .map(|(_, key, kc)| (key.clone(), kc.as_ref().map(|k| k.contexts.iter().map(|c| c.name.clone()).collect())))
        .collect();
    let kept = crate::settings::with(|s| s.context_files.clone());
    let mut owners = kept.clone();
    let now = name_owners(&mut owners, &names, |f| crate::paths::expand(f).exists(), |f| settled.contains(f));
    // A name kept for a file read without it: read again to settle it.
    let settled = !owners
        .iter()
        .any(|(name, file)| names.iter().any(|(f, n)| f == file && n.as_ref().is_some_and(|n| !holds(n, name))));
    let read = files.into_iter().filter_map(|(stem, key, kc)| Some((stem, key, kc?))).collect();
    let merged = merge_owned(read, &now, &mut owners);
    if owners != kept {
        crate::settings::update(|s| s.context_files = owners);
    }
    match (merged, last_err) {
        (Some(k), _) => Ok((k, settled)),
        (None, Some(e)) => Err(e).context("reading kubeconfig"),
        (None, None) => anyhow::bail!(
            "no kubeconfig found (KUBECONFIG or ~/.kube/config): add or paste one in Settings → Kubeconfig"
        ),
    }
}

/// One kubeconfig file, read as kube reads it. Its error names the file and says what failed,
/// never what the file holds (see `unquoted`): it reaches pages, agents and the audit log. Read
/// kubeconfig files with this, not `Kubeconfig::read_from`.
pub(super) fn read_kubeconfig_file(path: &std::path::Path) -> Result<Kubeconfig> {
    let file = || tilde(path);
    // kube reads a file that starts with a UTF-16 byte order mark two bytes at a time and panics
    // on an odd last byte (#890): such a file in a kubeconfig folder crashed the app.
    let bytes = std::fs::read(path).map_err(|e| anyhow!("{}: {e}", file()))?;
    if odd_utf16(&bytes) {
        return Err(anyhow!("{}: not valid UTF-16 (a byte order mark, then an odd number of bytes)", file()));
    }
    // kube reads the file again, and it may have changed since: its panic is an error too.
    std::panic::catch_unwind(|| Kubeconfig::read_from(path))
        .map_err(|_| anyhow!("{}: kube failed reading it", file()))?
        .map_err(|e| anyhow!("{}: {}", file(), unquoted(e)))
}

/// Whether `bytes` start with a UTF-16 byte order mark (little or big endian) and are of an odd
/// length, which kube's `read_from` panics on.
fn odd_utf16(bytes: &[u8]) -> bool {
    matches!(bytes, [0xFF, 0xFE, ..] | [0xFE, 0xFF, ..]) && bytes.len() % 2 == 1
}

/// Kubeconfig text pasted in Settings, read as `read_kubeconfig_file` reads a file.
pub(super) fn parse_kubeconfig(text: &str) -> Result<Kubeconfig> {
    Kubeconfig::from_yaml(text).map_err(|e| anyhow!(unquoted(e)))
}

/// What kube's error for a kubeconfig says, without the file's text: a parse error is said by
/// `parse_failure`, a read error by itself (the caller names the file). Its other errors, two
/// documents of a file that don't merge, hold no text of the file.
fn unquoted(e: kube::config::KubeconfigError) -> String {
    use kube::config::KubeconfigError as E;
    match e {
        E::Parse(e) => parse_failure(&e),
        E::ReadConfig(e, _) => e.to_string(),
        e => e.to_string(),
    }
}

/// Where a kubeconfig failed to parse, and why in words of our own (#860). serde-saphyr's error
/// shows the lines around the error, and its message can quote a value, a key or a tag
/// (`invalid type: string "…"`), the parser's a character: there a token, a client key, a
/// password or a login command's arguments can be. Only the line and column are kept.
fn parse_failure(e: &serde_saphyr::Error) -> String {
    use serde_saphyr::Error as E;
    // An alias fails as what it stands for.
    let mut inner = e.without_snippet();
    while let E::AliasError { error, .. } = inner {
        inner = error.without_snippet();
    }
    let why: String = match inner {
        // The YAML parser's own errors: an unclosed quote or bracket, a tab, a bad indentation.
        E::ExternalMessage { .. } | E::Eof { .. } | E::UnknownAnchor { .. } | E::InvalidUtf8Input => {
            "not valid YAML".into()
        }
        // A field of kube's types, not a word of the file.
        E::SerdeMissingField { field, .. } => format!("missing field `{field}`"),
        E::DuplicateMappingKey { .. } => "a key given twice".into(),
        _ => "an unexpected value".into(),
    };
    match e.location() {
        Some(at) => format!("{why} at line {}, column {}", at.line(), at.column()),
        None => why,
    }
}

/// The file that has each context name in this merge, given the files read now (by
/// path, with their context names, or `None` when unreadable) and `kept`, the files
/// that had each name before. `kept` is brought up to date: a file deleted, or read
/// without the name while `settled` (unchanged for a while, see `settle`), gives it up; one
/// that failed to read, or may be half-written, keeps it; a new name goes to the first
/// file that has it. A name kept for a file not merged now (another KUBECONFIG) goes to
/// the first file here that has it, without changing `kept`. `kept` also holds the
/// `name@file` names given by `merge_owned`: one stays while its file has `name`, and its
/// file keeps it rather than taking `name` when that is freed (`name` carries the settings
/// chosen for another cluster). A name only such files have has no owner here.
fn name_owners(
    kept: &mut std::collections::BTreeMap<String, String>,
    files: &[(String, Option<Vec<String>>)],
    exists: impl Fn(&str) -> bool,
    settled: impl Fn(&str) -> bool,
) -> HashMap<String, String> {
    let read: HashMap<&str, Option<&Vec<String>>> = files.iter().map(|(f, n)| (f.as_str(), n.as_ref())).collect();
    kept.retain(|name, file| match read.get(file.as_str()) {
        Some(Some(names)) => holds(names, name) || !settled(file.as_str()),
        Some(None) => true,
        None => exists(file.as_str()),
    });
    // The files with a `name@file` of their own for a name, by (file, name). A context the
    // file itself calls `name@x` was not given by `merge_owned`: the file can still take `name`.
    let renamed: std::collections::HashSet<(String, String)> = kept
        .iter()
        .flat_map(|(given, file)| {
            let names = read.get(file.as_str()).copied().flatten().map_or_default(Vec::as_slice);
            let own = names.contains(given);
            names
                .iter()
                .filter(move |n| !own && renamed_from(given, n).is_some())
                .map(move |n| (file.clone(), n.clone()))
        })
        .collect();
    let mut now = HashMap::new();
    for (file, names) in files {
        for name in names.iter().flatten() {
            if kept.get(name) != Some(file) && renamed.contains(&(file.clone(), name.clone())) {
                continue;
            }
            let owner = kept.entry(name.clone()).or_insert_with(|| file.clone());
            let owner = if read.contains_key(owner.as_str()) { owner.clone() } else { file.clone() };
            now.entry(name.clone()).or_insert(owner);
        }
    }
    now
}

/// Whether a file with context `names` holds the kept `name`: one of them, or the
/// `name@file` given to one (see `merge_owned`).
fn holds(names: &[String], name: &str) -> bool {
    names.iter().any(|n| n == name || renamed_from(name, n).is_some())
}

/// The `file` of `name` when it is `original@file`, a name `merge_owned` may give.
fn renamed_from<'a>(name: &'a str, original: &str) -> Option<&'a str> {
    name.strip_prefix(original)?.strip_prefix('@')
}

/// Merge files (file name, path, contents) in order. A context whose name `owners`
/// gives to another file, or to none, is renamed `name@file` first, or `name@file-2`…
/// when that is taken: the name is kept in `kept` (see `name_owners`), so it stays with that file
/// whatever the other files do. Clusters and users that clash are renamed as they are
/// merged (see `merge_unique`).
fn merge_owned(
    files: Vec<(String, String, Kubeconfig)>,
    owners: &HashMap<String, String>,
    kept: &mut std::collections::BTreeMap<String, String>,
) -> Option<Kubeconfig> {
    // Every context name read, and every name kept for a file, even one not read now.
    let mut taken: std::collections::HashSet<String> =
        files.iter().flat_map(|(_, _, kc)| kc.contexts.iter().map(|c| c.name.clone())).collect();
    taken.extend(kept.keys().cloned());
    let mut given = std::collections::HashSet::new();
    let mut merged: Option<Kubeconfig> = None;
    for (stem, key, mut more) in files {
        for c in &mut more.contexts {
            if owners.get(&c.name) != Some(&key) {
                // The name this context had before, else a new one, kept for this file.
                let before = kept.iter().find(|(name, file)| {
                    **file == key
                        && !given.contains(*name)
                        && renamed_from(name, &c.name).is_some_and(|f| {
                            f == stem
                                || f.strip_prefix(stem.as_str())
                                    .and_then(|n| n.strip_prefix('-'))
                                    .is_some_and(|i| i.parse::<u32>().is_ok())
                        })
                });
                let new = if let Some((name, _)) = before {
                    name.clone()
                } else {
                    let new = free_name(&c.name, &stem, &mut taken);
                    kept.insert(new.clone(), key.clone());
                    new
                };
                given.insert(new.clone());
                let old = std::mem::replace(&mut c.name, new);
                // The file's current context follows its rename.
                if more.current_context.as_deref() == Some(old.as_str()) {
                    more.current_context = Some(c.name.clone());
                }
            }
        }
        match merged.as_mut() {
            None => merged = Some(more),
            Some(k) => merge_unique(k, more, &stem),
        }
    }
    merged
}

/// Contexts from the kubeconfig files, cached: the files are only read again when
/// they change (checked at most every two seconds). Safe to call while rendering.
pub fn load_contexts() -> Result<Contexts> {
    cached_contexts(std::time::Duration::from_secs(2))
}

/// Like `load_contexts`, but checks the files now: for the watcher that just saw them change.
pub fn reload_contexts() -> Result<Contexts> {
    cached_contexts(std::time::Duration::ZERO)
}

fn cached_contexts(max_age: std::time::Duration) -> Result<Contexts> {
    type Cached = (std::time::Instant, Vec<(std::path::PathBuf, Option<std::time::SystemTime>, u64)>, Contexts);
    static CACHE: std::sync::Mutex<Option<Cached>> = std::sync::Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((checked, _, contexts)) = cache.as_ref()
        && checked.elapsed() < max_age
    {
        return Ok(contexts.clone());
    }
    let signature = kubeconfig_signature();
    if let Some((checked, sig, contexts)) = cache.as_mut()
        && *sig == signature
        && contexts.settled
    {
        *checked = std::time::Instant::now();
        return Ok(contexts.clone());
    }
    let contexts = read_contexts()?;
    *cache = Some((std::time::Instant::now(), signature, contexts.clone()));
    Ok(contexts)
}

fn read_contexts() -> Result<Contexts> {
    let (kc, settled) = read_kubeconfig()?;
    let mut names: Vec<String> = kc.contexts.iter().map(|c| c.name.clone()).collect();
    names.sort();
    let risky = risky_contexts(&kc);
    let fingerprints = kc.contexts.iter().filter_map(|c| Some((c.name.clone(), fingerprint(&kc, &c.name)?))).collect();
    Ok(Contexts { names, current: kc.current_context, risky, fingerprints, settled })
}

/// A hash of the context's entry with its cluster's and its user's: the same while the
/// context reaches the same server as the same user. None when it can't be computed.
pub(super) fn fingerprint(kc: &Kubeconfig, context: &str) -> Option<u64> {
    use std::hash::{Hash as _, Hasher as _};
    let ctx = kc.contexts.iter().find(|c| c.name == context)?.context.as_ref();
    let cluster = ctx.and_then(|c| kc.clusters.iter().find(|k| k.name == c.cluster));
    let user = ctx.and_then(|c| c.user.as_deref()).and_then(|u| kc.auth_infos.iter().find(|a| a.name == u));
    let text = serde_json::to_string(&(ctx, cluster, user)).ok()?;
    let mut hasher = std::hash::DefaultHasher::new();
    text.hash(&mut hasher);
    Some(hasher.finish())
}

/// Contexts that look like production or admin rights, from the context's name, its
/// user, its cluster's name and the server URL.
pub(super) fn risky_contexts(kc: &Kubeconfig) -> Vec<String> {
    kc.contexts
        .iter()
        .filter(|c| {
            let ctx = c.context.as_ref();
            let cluster = ctx.map_or_default(|x| x.cluster.as_str());
            let server = kc
                .clusters
                .iter()
                .find(|k| k.name == cluster)
                .and_then(|k| k.cluster.as_ref())
                .and_then(|k| k.server.as_deref())
                .unwrap_or_default();
            let user = ctx.and_then(|x| x.user.as_deref()).unwrap_or_default();
            looks_risky(&[&c.name, user, cluster, server])
        })
        .map(|c| c.name.clone())
        .collect()
}

/// "prod", "production", "prd", "admin", "root" anywhere; "live" as a word of its own
/// (not in "olive" or "delivery").
fn looks_risky(texts: &[&str]) -> bool {
    texts.iter().any(|t| {
        let t = t.to_lowercase();
        ["prod", "prd", "admin", "root"].iter().any(|w| t.contains(w))
            || t.split(|c: char| !c.is_ascii_alphanumeric()).any(|w| w == "live")
    })
}

/// The longest wait for a response to start (headers). It also covers a token refresh by an
/// exec plugin, which can be a browser login (kubelogin): long enough for a person to log in.
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(180);

/// A client for the UI: its writes and shells need write mode when they are sent
/// ([`gate::WriteGate`]).
pub async fn connect(context: String) -> Result<Client> {
    connect_with(context, true).await
}

/// A client for the agents' MCP server: their rules decide what it may do, not the UI's
/// write mode.
pub async fn connect_for_agents(context: String) -> Result<Client> {
    connect_with(context, false).await
}

async fn connect_with(context: String, gated: bool) -> Result<Client> {
    run(async move {
        let kc = kubeconfig()?;
        // Building the client runs the context's exec plugin. With an expired token,
        // `kubelogin` and the like open a browser login on each run: let one run log in
        // and cache the token, the others then find it.
        let _login = match exec_command(&kc, &context) {
            Some(cmd) => Some(exec_lock(cmd).lock_owned().await),
            None => None,
        };
        let proxy_cut = proxy_text(&kc, &context).is_some_and(|text| text.contains('#'));
        let opts = KubeConfigOptions { context: Some(context.clone()), ..Default::default() };
        let config =
            Config::from_custom_kubeconfig(kc, &opts).await.with_context(|| format!("loading context {context}"))?;
        // The exec plugin runs as a blocking process here (a browser login can take minutes):
        // off the shared runtime's few threads, so other clusters keep streaming meanwhile.
        tokio::task::spawn_blocking(move || client(config, context, gated, proxy_cut))
            .await
            .context("building the client")?
    })
    .await
}

/// The proxy's text, as written, that kube reads for `context`: its cluster's `proxy-url`, else
/// `HTTPS_PROXY`, else `https_proxy` (the first that isn't empty). kube keeps it as an
/// `http::Uri`, which drops what follows a `#`: only this text tells that a password was cut
/// there (see `login::build_error`).
fn proxy_text(kc: &Kubeconfig, context: &str) -> Option<String> {
    let cluster = kc.contexts.iter().find(|c| c.name == context)?.context.as_ref().map(|c| &c.cluster);
    let from_kubeconfig = cluster
        .and_then(|name| kc.clusters.iter().find(|k| &k.name == name))
        .and_then(|k| k.cluster.as_ref())
        .and_then(|k| k.proxy_url.clone());
    [from_kubeconfig, std::env::var("HTTPS_PROXY").ok(), std::env::var("https_proxy").ok()]
        .into_iter()
        .flatten()
        .find(|text| !text.is_empty())
}

/// A context's client from its config, with the app's layers. Runs the exec plugin, if any:
/// when it fails, here or on a later request, the error says its program and its error only
/// (`login`), not its command line or output. `proxy_cut`: the proxy's text holds a `#` (see
/// `proxy_text`).
fn client(mut config: Config, context: String, gated: bool, proxy_cut: bool) -> Result<Client> {
    // kube's retry sends a 503 again for about three minutes: an aggregated API that is down
    // (metrics-server) held pages that long. `retry` only waits when the server says when.
    config.default_retry = false;
    // kube sets no read timeout: on a connection that died without a word (sleep, VPN,
    // NAT), a request would wait forever. This bounds the wait for the response headers;
    // watches, logs and shells stream after that and are not cut.
    let timeout = tower::timeout::TimeoutLayer::new(REQUEST_TIMEOUT);
    // Outside the timeout, so a request it cuts is counted as failed.
    let write_gate = gate::WriteGateLayer { context: context.as_str().into() };
    let meter = meter::MeterLayer { context };
    // kube's error for a failed login command names its program among its arguments, or not at
    // all (when it could not be started): it is named from the kubeconfig instead.
    let program = login::program(&config.auth_info);
    let builder = kube::client::ClientBuilder::try_from(config)
        .map_err(|e| login::build_error(e, program.as_deref(), proxy_cut))?;
    // Right around kube's own layers, where a failed token refresh comes out: the app's layers
    // and every caller only see it said by `login`.
    let builder = builder
        .with_layer(&login::LoginErrorLayer::new(program.as_deref()))
        .with_layer(&retry::RetryLayer)
        .with_layer(&timeout)
        .with_layer(&meter);
    Ok(if gated { builder.with_layer(&write_gate).build() } else { builder.build() })
}

/// Where a context points: its kubeconfig cluster's name and server URL, and its user's
/// exec plugin command line (None without one).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextTarget {
    pub name: String,
    pub cluster: String,
    pub server: Option<String>,
    pub exec: Option<String>,
}

/// What each context of the merged kubeconfig points to.
pub fn context_targets() -> Result<Vec<ContextTarget>> {
    Ok(targets_of(&kubeconfig()?))
}

fn targets_of(kc: &Kubeconfig) -> Vec<ContextTarget> {
    kc.contexts
        .iter()
        .map(|c| {
            let cluster = c.context.as_ref().map_or_default(|x| x.cluster.clone());
            let server = kc
                .clusters
                .iter()
                .find(|k| k.name == cluster)
                .and_then(|k| k.cluster.as_ref())
                .and_then(|k| k.server.clone());
            ContextTarget { name: c.name.clone(), cluster, server, exec: exec_command(kc, &c.name) }
        })
        .collect()
}

/// The exec plugin command line of a context's user, if it uses one.
fn exec_command(kc: &Kubeconfig, context: &str) -> Option<String> {
    let user = kc.contexts.iter().find(|c| c.name == context)?.context.as_ref()?.user.as_ref()?;
    let exec = kc.auth_infos.iter().find(|a| &a.name == user)?.auth_info.as_ref()?.exec.as_ref()?;
    let mut line = vec![exec.command.clone()?];
    line.extend(exec.args.iter().flatten().cloned());
    Some(line.join(" "))
}

/// One lock per exec plugin command line, so contexts that share a login wait for each other.
fn exec_lock(cmd: String) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: LazyLock<std::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>> =
        LazyLock::new(Default::default);
    LOCKS.lock().unwrap_or_else(std::sync::PoisonError::into_inner).entry(cmd).or_default().clone()
}

/// A copy of the merged kubeconfig with `context` (and `namespace`) selected,
/// readable only by the user, for tools run on this machine (shell, helm).
/// Each call gets its own file, so concurrent uses never share or overwrite one. It is
/// removed when dropped: keep it alive as long as the process using it.
pub fn private_kubeconfig(context: &str, namespace: Option<&str>, purpose: &str) -> Result<TempFile> {
    let mut kc = kubeconfig()?;
    kc.current_context = Some(context.to_string());
    // Only this context, its cluster and its user: a plugin or helm never sees other clusters' credentials.
    kc.contexts.retain(|c| c.name == context);
    let (cluster, user) =
        kc.contexts.first().and_then(|c| c.context.as_ref()).map_or_default(|c| (c.cluster.clone(), c.user.clone()));
    kc.clusters.retain(|c| c.name == cluster);
    kc.auth_infos.retain(|a| Some(&a.name) == user.as_ref());
    if let Some(ns) = namespace {
        for c in kc.contexts.iter_mut().filter(|c| c.name == context) {
            if let Some(ctx) = c.context.as_mut() {
                ctx.namespace = Some(ns.to_string());
            }
        }
    }
    Ok(TempFile::in_runtime_dir(purpose, "yaml", crate::yaml::to_string(&kc)?.as_bytes())?)
}

/// Longest ServiceAccount token lifetime handed out, in hours.
pub const MAX_TOKEN_HOURS: i64 = 24;

/// A piece of a file name: anything but letters, digits, `.`, `_` and `-` becomes `_`
/// (EKS context names hold `:` and `/`; `../` must not leave the folder).
pub fn safe_file_part(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect()
}

/// A kubeconfig that authenticates as a ServiceAccount, with a token from the
/// TokenRequest API (expires after `hours`, at most a day). Saved to ~/Downloads, private.
pub async fn service_account_kubeconfig(
    client: Client,
    context: String,
    ns: String,
    sa: String,
    hours: i64,
) -> Result<std::path::PathBuf> {
    let hours = hours.clamp(1, MAX_TOKEN_HOURS);
    let kc = kubeconfig()?;
    let ctx = kc
        .contexts
        .iter()
        .find(|c| c.name == context)
        .and_then(|c| c.context.clone())
        .context("context not in kubeconfig")?;
    let cluster = kc
        .clusters
        .iter()
        .find(|c| c.name == ctx.cluster)
        .and_then(|c| c.cluster.clone())
        .context("cluster not in kubeconfig")?;
    let token = run(async move {
        let body = serde_json::json!({"apiVersion": "authentication.k8s.io/v1", "kind": "TokenRequest", "spec": {"expirationSeconds": hours * 3600}});
        let req = http::Request::post(format!("/api/v1/namespaces/{ns}/serviceaccounts/{sa}/token")).header("content-type", "application/json").body(serde_json::to_vec(&body)?)?;
        let resp: serde_json::Value = client.request(req).await?;
        let token = resp.pointer("/status/token").and_then(|v| v.as_str()).context("no token returned")?.to_string();
        Ok((token, ns, sa))
    })
    .await?;
    let (token, ns, sa) = token;
    let name = format!("{context}-{ns}-{sa}");
    let c = standalone_cluster(&cluster);
    let config = serde_json::json!({
        "apiVersion": "v1",
        "kind": "Config",
        "clusters": [{"name": name, "cluster": c}],
        "users": [{"name": name, "user": {"token": token}}],
        "contexts": [{"name": name, "context": {"cluster": name, "user": name, "namespace": ns}}],
        "current-context": name,
    });
    let stem = format!("kubeconfig-{}", safe_file_part(&name));
    Ok(save_new(&crate::paths::downloads_dir(), &stem, "yaml", crate::yaml::to_string(&config)?.as_bytes())?)
}

/// A kubeconfig cluster entry that works on its own: the CA inlined, the TLS and proxy
/// settings kept.
fn standalone_cluster(cluster: &kube::config::Cluster) -> serde_json::Value {
    let mut c = serde_json::json!({"server": cluster.server});
    if let Some(ca) = &cluster.certificate_authority_data {
        c["certificate-authority-data"] = ca.clone().into();
    } else if let Some(path) = &cluster.certificate_authority
        && let Ok(pem) = std::fs::read(path)
    {
        use base64::Engine as _;
        c["certificate-authority-data"] = base64::engine::general_purpose::STANDARD.encode(pem).into();
    }
    if cluster.insecure_skip_tls_verify == Some(true) {
        c["insecure-skip-tls-verify"] = true.into();
    }
    if let Some(name) = &cluster.tls_server_name {
        c["tls-server-name"] = name.clone().into();
    }
    if let Some(p) = &cluster.proxy_url {
        c["proxy-url"] = p.clone().into();
    }
    c
}

#[cfg(test)]
mod kubeconfig_tests {
    #[test]
    fn guesses_production_and_admin() {
        let kc: kube::config::Kubeconfig = crate::yaml::from_str(
            "clusters:\n\
             - {name: c1, cluster: {server: 'https://api.prd.example.com'}}\n\
             - {name: c2, cluster: {server: 'https://10.0.0.1'}}\n\
             - {name: live, cluster: {server: 'https://10.0.0.2'}}\n\
             - {name: c4, cluster: {server: 'https://delivery.example.com'}}\n\
             users: [{name: me, user: {token: a}}, {name: cluster-admin, user: {token: b}}]\n\
             contexts:\n\
             - {name: eu, context: {cluster: c1, user: me}}\n\
             - {name: dev, context: {cluster: c2, user: me}}\n\
             - {name: shop, context: {cluster: live, user: me}}\n\
             - {name: olive, context: {cluster: c4, user: me}}\n\
             - {name: lab, context: {cluster: c2, user: cluster-admin}}\n\
             - {name: Production-EU, context: {cluster: c2, user: me}}\n",
        )
        .unwrap();
        assert_eq!(super::risky_contexts(&kc), ["eu", "shop", "lab", "Production-EU"]);
    }

    #[test]
    fn file_names_stay_in_their_folder() {
        assert_eq!(
            super::safe_file_part("arn:aws:eks:eu-west-1:123:cluster/prod"),
            "arn_aws_eks_eu-west-1_123_cluster_prod"
        );
        assert_eq!(super::safe_file_part("../../.ssh/x"), ".._.._.ssh_x");
    }

    #[test]
    fn downloaded_cluster_keeps_tls_settings() {
        let kc: kube::config::Kubeconfig = crate::yaml::from_str(
            "clusters: [{name: c, cluster: {server: 'https://10.0.0.1:6443', certificate-authority-data: Q0E=, tls-server-name: api.example.com, proxy-url: 'socks5://localhost:1080'}}]\n",
        )
        .unwrap();
        let c = super::standalone_cluster(kc.clusters[0].cluster.as_ref().unwrap());
        assert_eq!(c["server"], "https://10.0.0.1:6443");
        assert_eq!(c["certificate-authority-data"], "Q0E=");
        assert_eq!(c["tls-server-name"], "api.example.com");
        assert_eq!(c["proxy-url"], "socks5://localhost:1080");
        assert!(c.get("insecure-skip-tls-verify").is_none());
    }

    #[test]
    fn same_names_stay_apart() {
        let a: kube::config::Kubeconfig = crate::yaml::from_str("clusters: [{name: prod, cluster: {server: https://a}}]\nusers: [{name: me, user: {token: a}}]\ncontexts: [{name: prod, context: {cluster: prod, user: me}}]\ncurrent-context: prod\n").unwrap();
        let b: kube::config::Kubeconfig = crate::yaml::from_str("clusters: [{name: prod, cluster: {server: https://b}}]\nusers: [{name: me, user: {token: b}}]\ncontexts: [{name: prod, context: {cluster: prod, user: me}}]\n").unwrap();
        let mut kc = a;
        super::merge_unique(&mut kc, b, "team-b");
        let names: Vec<&str> = kc.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["prod", "prod@team-b"]);
        let ctx = kc.contexts[1].context.as_ref().unwrap();
        assert_eq!(ctx.cluster, "prod@team-b");
        assert_eq!(ctx.user.as_deref(), Some("me@team-b"));
        let server = kc
            .clusters
            .iter()
            .find(|c| c.name == "prod@team-b")
            .and_then(|c| c.cluster.as_ref())
            .and_then(|c| c.server.clone());
        assert_eq!(server.as_deref(), Some("https://b"));
        assert_eq!(kc.current_context.as_deref(), Some("prod"));
    }

    #[test]
    fn current_context_follows_its_rename() {
        let a: kube::config::Kubeconfig =
            crate::yaml::from_str("contexts: [{name: prod, context: {cluster: a}}]\n").unwrap();
        let b: kube::config::Kubeconfig =
            crate::yaml::from_str("contexts: [{name: prod, context: {cluster: b}}]\ncurrent-context: prod\n").unwrap();
        let mut kc = a;
        super::merge_unique(&mut kc, b, "team-b");
        assert_eq!(kc.current_context.as_deref(), Some("prod@team-b"));
    }

    /// A file read now, with its context names.
    fn read(file: &str, names: &[&str]) -> (String, Option<Vec<String>>) {
        (file.to_string(), Some(names.iter().map(ToString::to_string).collect()))
    }

    #[test]
    fn a_name_stays_with_the_file_that_had_it_first() {
        let mut kept = std::collections::BTreeMap::new();
        let on_disk = |_: &str| true;
        // a.yaml was there first.
        let now = super::name_owners(&mut kept, &[read("a", &["prod"])], on_disk, |_| true);
        assert_eq!(now["prod"], "a");
        // b.yaml comes, then the files are read in another order (a saved again, new birth time).
        super::name_owners(&mut kept, &[read("a", &["prod"]), read("b", &["prod"])], on_disk, |_| true);
        let now = super::name_owners(&mut kept, &[read("b", &["prod"]), read("a", &["prod"])], on_disk, |_| true);
        assert_eq!(now["prod"], "a");
        // a briefly unreadable: b doesn't take the name.
        let now = super::name_owners(&mut kept, &[(String::from("a"), None), read("b", &["prod"])], on_disk, |_| true);
        assert_eq!(now["prod"], "a");
        assert_eq!(kept["prod"], "a");
        // A name added to an earlier file (the default kubeconfig) doesn't take it either.
        let now = super::name_owners(&mut kept, &[read("c", &["prod"]), read("a", &["prod"])], on_disk, |_| true);
        assert_eq!(now["prod"], "a");
        // Read without the name, a gives it up; the next file that has it keeps it.
        let now = super::name_owners(&mut kept, &[read("a", &[]), read("b", &["prod"])], on_disk, |_| true);
        assert_eq!(now["prod"], "b");
        let now = super::name_owners(&mut kept, &[read("a", &["prod"]), read("b", &["prod"])], on_disk, |_| true);
        assert_eq!(now["prod"], "b");
        // A file not read now (another KUBECONFIG) keeps its names for later.
        let now = super::name_owners(&mut kept, &[read("c", &["prod"])], on_disk, |_| true);
        assert_eq!(now["prod"], "c");
        assert_eq!(kept["prod"], "b");
        // Deleted, it gives them up.
        let now = super::name_owners(&mut kept, &[read("c", &["prod"])], |_| false, |_| true);
        assert_eq!(now["prod"], "c");
        assert_eq!(kept["prod"], "c");
    }

    #[test]
    fn a_file_settles_after_a_while_unchanged() {
        let mut seen = std::collections::HashMap::new();
        let t = std::time::Instant::now();
        let s = |secs| t + std::time::Duration::from_secs(secs);
        let stamp = |len| Some((None, len));
        // Truncated, then read again right away by another caller: not settled yet.
        assert!(!super::settle(&mut seen, "a", stamp(0), stamp(0), s(0)));
        assert!(!super::settle(&mut seen, "a", stamp(0), stamp(0), s(1)));
        // Filled in: the time starts again.
        assert!(!super::settle(&mut seen, "a", stamp(9), stamp(9), s(2)));
        assert!(!super::settle(&mut seen, "a", stamp(9), stamp(9), s(3)));
        assert!(super::settle(&mut seen, "a", stamp(9), stamp(9), s(4)));
        // Changed during the read, or gone.
        assert!(!super::settle(&mut seen, "a", stamp(5), stamp(9), s(9)));
        assert!(!super::settle(&mut seen, "a", stamp(9), None, s(9)));
        assert!(!seen.contains_key("a"));
    }

    #[test]
    fn a_context_named_like_a_rename_does_not_hold_back_its_file() {
        let mut kept = std::collections::BTreeMap::new();
        let on_disk = |_: &str| true;
        super::name_owners(&mut kept, &[read("a", &["prod@eu"])], on_disk, |_| true);
        // a gets a context `prod` next to its own `prod@eu`: it is shown as `prod`.
        let now = super::name_owners(&mut kept, &[read("a", &["prod@eu", "prod"])], on_disk, |_| true);
        assert_eq!(now["prod"], "a");
        assert_eq!(now["prod@eu"], "a");
    }

    #[test]
    fn a_half_written_file_keeps_its_names() {
        let mut kept = std::collections::BTreeMap::new();
        let on_disk = |_: &str| true;
        super::name_owners(&mut kept, &[read("a", &["prod"]), read("b", &["prod"])], on_disk, |_| true);
        // `kubectl config use-context` truncated a: read in between, it has no contexts.
        let now = super::name_owners(&mut kept, &[read("a", &[]), read("b", &["prod"])], on_disk, |f| f != "a");
        assert_eq!(now["prod"], "a");
        assert_eq!(kept["prod"], "a");
        // Read the same way again, a really gave it up.
        let now = super::name_owners(&mut kept, &[read("a", &[]), read("b", &["prod"])], on_disk, |_| true);
        assert_eq!(now["prod"], "b");
        assert_eq!(kept["prod"], "b");
    }

    #[test]
    fn the_file_with_the_name_keeps_it_when_merged_later() {
        let file = |server: &str, current: bool| -> kube::config::Kubeconfig {
            let current = if current { "current-context: prod\n" } else { "" };
            crate::yaml::from_str(&format!("clusters: [{{name: prod, cluster: {{server: {server}}}}}]\nusers: [{{name: me, user: {{token: {server}}}}}]\ncontexts: [{{name: prod, context: {{cluster: prod, user: me}}}}]\n{current}")).unwrap()
        };
        let owners = std::collections::HashMap::from([("prod".to_string(), "a".to_string())]);
        let files =
            vec![("b".into(), "b".into(), file("https://b", true)), ("a".into(), "a".into(), file("https://a", false))];
        let kc = super::merge_owned(files, &owners, &mut Default::default()).unwrap();
        let server = |ctx: &str| {
            let cluster = &kc.contexts.iter().find(|c| c.name == ctx).unwrap().context.as_ref().unwrap().cluster;
            kc.clusters
                .iter()
                .find(|c| &c.name == cluster)
                .and_then(|c| c.cluster.as_ref())
                .and_then(|c| c.server.clone())
        };
        assert_eq!(server("prod").as_deref(), Some("https://a"));
        assert_eq!(server("prod@b").as_deref(), Some("https://b"));
        assert_eq!(kc.current_context.as_deref(), Some("prod@b"));
    }

    #[test]
    fn a_renamed_name_stays_with_its_file() {
        let file = |server: &str| -> kube::config::Kubeconfig {
            crate::yaml::from_str(&format!("clusters: [{{name: x, cluster: {{server: {server}}}}}]\ncontexts: [{{name: x, context: {{cluster: x}}}}]\n")).unwrap()
        };
        // Three kubeadm `admin.conf` files: each read now, or `None` when unreadable.
        let merge = |kept: &mut std::collections::BTreeMap<String, String>,
                     files: Vec<(&str, Option<kube::config::Kubeconfig>)>| {
            let names: Vec<(String, Option<Vec<String>>)> = files
                .iter()
                .map(|(f, kc)| {
                    (f.to_string(), kc.as_ref().map(|k| k.contexts.iter().map(|c| c.name.clone()).collect()))
                })
                .collect();
            // A file not listed is deleted.
            let now = super::name_owners(kept, &names, |_| false, |_| true);
            let read =
                files.into_iter().filter_map(|(f, kc)| Some(("admin".to_string(), f.to_string(), kc?))).collect();
            let kc = super::merge_owned(read, &now, kept).unwrap();
            kc.contexts
                .iter()
                .map(|c| {
                    let cluster = &c.context.as_ref().unwrap().cluster;
                    let k = kc.clusters.iter().find(|k| &k.name == cluster).unwrap();
                    (c.name.clone(), k.cluster.as_ref().and_then(|k| k.server.clone()).unwrap())
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let mut kept = Default::default();
        let all = |y: Option<_>| vec![("/w", Some(file("https://w"))), ("/y", y), ("/z", Some(file("https://z")))];
        let names = merge(&mut kept, all(Some(file("https://y"))));
        assert_eq!(names["x"], "https://w");
        assert_eq!(names["x@admin"], "https://y");
        assert_eq!(names["x@admin-2"], "https://z");
        // y can't be read: z keeps its name.
        let names = merge(&mut kept, all(None));
        assert_eq!(names.get("x@admin"), None);
        assert_eq!(names["x@admin-2"], "https://z");
        // Read in another order, back again.
        let mut files = all(Some(file("https://y")));
        files.reverse();
        let names = merge(&mut kept, files);
        assert_eq!(names["x"], "https://w");
        assert_eq!(names["x@admin"], "https://y");
        assert_eq!(names["x@admin-2"], "https://z");
        // y deleted: its name is freed, z still keeps its own.
        let names = merge(&mut kept, vec![("/w", Some(file("https://w"))), ("/z", Some(file("https://z")))]);
        assert_eq!(names["x@admin-2"], "https://z");
        assert!(!kept.contains_key("x@admin"));
        // w drops x: z keeps its own name rather than taking w's, and its settings.
        let empty: kube::config::Kubeconfig = crate::yaml::from_str("contexts: []\n").unwrap();
        let names = merge(&mut kept, vec![("/w", Some(empty)), ("/z", Some(file("https://z")))]);
        assert_eq!(names.get("x"), None);
        assert_eq!(names["x@admin-2"], "https://z");
        // w has it again: it is w's.
        let names = merge(&mut kept, vec![("/w", Some(file("https://w"))), ("/z", Some(file("https://z")))]);
        assert_eq!(names["x"], "https://w");
        assert_eq!(names["x@admin-2"], "https://z");
    }

    #[test]
    fn same_file_names_stay_apart() {
        let file = |server: &str| -> kube::config::Kubeconfig {
            crate::yaml::from_str(&format!("clusters: [{{name: kind, cluster: {{server: {server}}}}}]\nusers: [{{name: admin, user: {{token: {server}}}}}]\ncontexts: [{{name: kind, context: {{cluster: kind, user: admin}}}}]\n")).unwrap()
        };
        // ~/a/config, ~/b/config and ~/c/config: all three have the stem "config".
        let mut kc = file("https://a");
        super::merge_unique(&mut kc, file("https://b"), "config");
        super::merge_unique(&mut kc, file("https://c"), "config");
        let names: Vec<&str> = kc.contexts.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["kind", "kind@config", "kind@config-2"]);
        let ctx = kc.contexts[2].context.as_ref().unwrap();
        assert_eq!(ctx.cluster, "kind@config-2");
        assert_eq!(ctx.user.as_deref(), Some("admin@config-2"));
        let server = kc
            .clusters
            .iter()
            .find(|c| c.name == ctx.cluster)
            .and_then(|c| c.cluster.as_ref())
            .and_then(|c| c.server.clone());
        assert_eq!(server.as_deref(), Some("https://c"));
        assert_eq!(kc.clusters.len(), 3);
        assert_eq!(kc.auth_infos.len(), 3);
    }
}

#[cfg(test)]
mod tests {
    use super::exec_command;
    use kube::config::Kubeconfig;

    /// An aggregated API whose backend is down answers 503 at once: so must the client,
    /// not after minutes of retries (#742).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_503_comes_back_at_once() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf).await;
                let body = r#"{"kind":"Status","apiVersion":"v1","status":"Failure","message":"service unavailable","reason":"ServiceUnavailable","code":503}"#;
                let res = format!(
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                let _ = conn.write_all(res.as_bytes()).await;
            }
        });
        let config = kube::Config::new(format!("http://{addr}").parse().unwrap());
        let client = super::client(config, "test".into(), false, false).unwrap();
        let read =
            client.request_text(http::Request::get("/apis/metrics.k8s.io/v1beta1/nodes").body(Vec::new()).unwrap());
        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), read).await;
        let err = answer.expect("the 503 should come back at once").expect_err("a 503 is an error");
        assert!(matches!(err, kube::Error::Api(ref s) if s.code == 503), "{err}");
    }

    /// The meter is outside kube's decompression: a gzipped answer counts the bytes it
    /// decompresses to, and one time to its last byte once it is read.
    #[tokio::test(flavor = "multi_thread")]
    async fn the_meter_counts_an_answer_decompressed() {
        use std::io::Write as _;
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
        let text = "x".repeat(10_000);
        let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
        gz.write_all(text.as_bytes()).unwrap();
        let body = gz.finish().unwrap();
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut conn, _)) = listener.accept().await {
                let mut buf = [0u8; 4096];
                let _ = conn.read(&mut buf).await;
                let head = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-encoding: gzip\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                    body.len()
                );
                let _ = conn.write_all(head.as_bytes()).await;
                let _ = conn.write_all(&body).await;
            }
        });
        let config = kube::Config::new(format!("http://{addr}").parse().unwrap());
        let client = super::client(config, "gzip-test".into(), false, false).unwrap();
        let get = http::Request::get("/api/v1/namespaces/shop/configmaps/big").body(Vec::new()).unwrap();
        let read = tokio::time::timeout(std::time::Duration::from_secs(5), client.request_text(get)).await;
        assert_eq!(read.expect("the answer should come at once").unwrap(), text);
        let snapshot = super::meter::snapshot(super::meter::now_minute());
        let (_, _, stats) = snapshot
            .classes
            .iter()
            .find(|(c, k, _)| c == "gzip-test" && *k == super::meter::Class::List)
            .expect("the request is counted");
        assert_eq!((stats.count, stats.bytes, stats.last_byte.count()), (1, 10_000, 1));
        let heavy = snapshot.heaviest.iter().find(|h| h.context == "gzip-test").expect("the request is listed");
        assert_eq!((heavy.path.as_str(), heavy.bytes), ("/api/v1/namespaces/{namespace}/configmaps/{name}", 10_000));
    }

    /// A context whose login command is `sh -c script`, given an OIDC client secret in its
    /// arguments as kubelogin is and another in its environment, with `MARK` set to `mark`.
    /// Its stderr is kept, not shown.
    fn login_by(script: &str, mark: &std::path::Path) -> kube::Config {
        let var = |name: &str, value: &str| {
            std::collections::HashMap::from([
                ("name".to_string(), name.to_string()),
                ("value".to_string(), value.to_string()),
            ])
        };
        let mut config = kube::Config::new("http://127.0.0.1:1".parse().unwrap());
        config.auth_info.exec = Some(kube::config::ExecConfig {
            api_version: Some("client.authentication.k8s.io/v1".into()),
            command: Some("sh".into()),
            args: Some(vec!["-c".into(), script.into(), "login".into(), "--oidc-client-secret=s3cr3t-arg".into()]),
            env: Some(vec![var("LOGIN_ENV", "s3cr3t-env"), var("MARK", &mark.to_string_lossy())]),
            drop_env: Some(vec!["S3CR3T_DROPPED".into()]),
            interactive_mode: Some(kube::config::ExecInteractiveMode::Never),
            ..Default::default()
        });
        config
    }

    const LOGIN_FAILED: &str = "the login command `sh` failed (exit 3): error: refresh token expired";

    /// A login command that fails while the client is built: the error says its program, exit
    /// code and last stderr line, never its arguments, environment or output (#827).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_login_command_keeps_its_secrets() {
        let script = "echo \"token: $LOGIN_ENV\"; echo 'warning: no cached token' >&2; \
                      echo 'error: refresh token expired' >&2; exit 3";
        let mark = std::env::temp_dir().join("recouvrance-login-unused");
        let Err(e) = super::client(login_by(script, &mark), "test".into(), false, false) else {
            panic!("the login command fails");
        };
        assert_eq!(crate::k8s::short_error(&e), LOGIN_FAILED);
        assert_eq!(format!("{e:#}"), LOGIN_FAILED);
        assert!(!format!("{e:?}").contains("s3cr3t"), "{e:?}");
    }

    /// One that fails when it runs again to refresh the token, on a request: the same (#827).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_login_command_failing_on_refresh_keeps_its_secrets() {
        let mark = std::env::temp_dir().join(format!("recouvrance-login-{}", uuid::Uuid::new_v4().simple()));
        // The first run logs in, with a token expired already; every run after fails.
        let script = r#"if [ -e "$MARK" ]; then echo "$LOGIN_ENV"; echo 'error: refresh token expired' >&2; exit 3; fi
touch "$MARK"
echo '{"apiVersion":"client.authentication.k8s.io/v1","kind":"ExecCredential","status":{"token":"s3cr3t-token","expirationTimestamp":"2020-01-01T00:00:00Z"}}'"#;
        let client = super::client(login_by(script, &mark), "test".into(), false, false).unwrap();
        let read = client.request_text(http::Request::get("/version").body(Vec::new()).unwrap()).await;
        let _ = std::fs::remove_file(mark);
        let e = read.expect_err("the token refresh fails");
        assert_eq!(e.to_string(), format!("ServiceError: {LOGIN_FAILED}"));
        assert!(!format!("{e:?}").contains("s3cr3t"), "{e:?}");
        assert_eq!(crate::k8s::short_error(&anyhow::Error::from(e)), LOGIN_FAILED);
    }

    /// A login command that is not there, as when it is not on the `PATH` the app was started
    /// with: the error names it and says why it could not start (#827).
    #[tokio::test(flavor = "multi_thread")]
    async fn a_login_command_that_is_not_there_is_named() {
        let mut config = login_by("exit 0", &std::env::temp_dir().join("recouvrance-login-unused"));
        config.auth_info.exec.as_mut().unwrap().command = Some("/nonexistent/recouvrance-login".into());
        let Err(e) = super::client(config, "test".into(), false, false) else {
            panic!("the login command is not there");
        };
        let said = crate::k8s::short_error(&e);
        assert!(said.starts_with("the login command `recouvrance-login` could not be started: "), "{said}");
        // ERROR_PATH_NOT_FOUND on Windows, where the folder is missing; ENOENT elsewhere.
        assert!(said.contains(if cfg!(windows) { "os error 3" } else { "os error 2" }), "{said}");
    }

    /// A kubeconfig that fails to parse is said by its file, line and column, and why in words
    /// of our own: serde-saphyr's error showed the lines around it, here its token and client
    /// key, and quoted a value of the wrong type (#860).
    #[test]
    fn a_kubeconfig_that_fails_to_parse_is_said_without_what_it_holds() {
        let dir = crate::mcp::test_dir();
        let path = dir.join("config");
        let file = crate::paths::tilde(&path);
        let said = |text: &str| {
            std::fs::write(&path, text).unwrap();
            let e = super::read_kubeconfig_file(&path).unwrap_err();
            let all = format!("{e}\n{e:#}\n{e:?}");
            assert!(!all.contains("SECRET"), "{all}");
            e.to_string()
        };
        // `@` can't start a value: the broken line is between the token and the key.
        let broken = "apiVersion: v1\nkind: Config\nusers:\n- name: me\n  user:\n    token: SECRET-TOKEN\n    \
                      username: @me\n    client-key-data: SECRET-KEY\n";
        assert_eq!(said(broken), format!("{file}: not valid YAML at line 7, column 15"));
        let wrong = said("users:\n- name: me\n  user: SECRET-TOKEN\n");
        assert!(wrong.starts_with(&format!("{file}: an unexpected value")), "{wrong}");
        let twice = said("users:\n- name: me\n  user:\n    token: SECRET-TOKEN\n    token: SECRET-OTHER\n");
        assert!(twice.starts_with(&format!("{file}: a key given twice at line 5")), "{twice}");
        let unnamed = said("contexts:\n- context: {cluster: SECRET-CLUSTER}\n");
        assert!(unnamed.starts_with(&format!("{file}: missing field `name`")), "{unnamed}");
        // Pasted text is read the same way, without a file to name.
        let pasted = super::parse_kubeconfig(broken).unwrap_err();
        assert_eq!(format!("{pasted:#}"), "not valid YAML at line 7, column 15");
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A file that starts with a UTF-16 byte order mark and has an odd number of bytes made kube
    /// panic, and the app with it (#890): it is said not to read, naming the file. An even one
    /// reads as before.
    #[test]
    fn an_odd_utf16_file_fails_without_a_crash() {
        let dir = crate::mcp::test_dir();
        let path = dir.join("config");
        let file = crate::paths::tilde(&path);
        let utf16 = |mark: [u8; 2], big: bool| {
            let text = "apiVersion: v1\nkind: Config\ncurrent-context: c\n";
            let units = text.encode_utf16().flat_map(|u| if big { u.to_be_bytes() } else { u.to_le_bytes() });
            mark.into_iter().chain(units).collect::<Vec<u8>>()
        };
        for (mark, big) in [([0xFF, 0xFE], false), ([0xFE, 0xFF], true)] {
            let mut bytes = utf16(mark, big);
            std::fs::write(&path, &bytes).unwrap();
            let kc = super::read_kubeconfig_file(&path).unwrap();
            assert_eq!(kc.current_context.as_deref(), Some("c"));
            bytes.push(b'x');
            std::fs::write(&path, &bytes).unwrap();
            let e = super::read_kubeconfig_file(&path).unwrap_err();
            assert_eq!(
                e.to_string(),
                format!("{file}: not valid UTF-16 (a byte order mark, then an odd number of bytes)")
            );
        }
        std::fs::remove_dir_all(dir).unwrap();
    }

    /// A proxy kube can't use, by its scheme or for having none (an `HTTPS_PROXY` that curl
    /// reads as http): the error of building the client names it by its host and port, never
    /// its user and password, however it is printed (#859).
    #[test]
    fn a_proxy_s_password_stays_out_of_the_connect_error() {
        let cases = [
            ("socks5h://user:secret@proxy.corp:1080", "socks5h://proxy.corp:1080/"),
            ("user:secret@proxy.corp:3128", "proxy.corp:3128"),
        ];
        for (url, shown) in cases {
            let mut config = kube::Config::new("https://127.0.0.1:1".parse().unwrap());
            config.proxy_url = Some(url.parse().unwrap());
            // kube's own error prints the URL whole.
            let Err(raw) = kube::Client::try_from(config.clone()) else { panic!("kube uses {url}") };
            assert!(raw.to_string().contains("user:secret@"), "{raw}");
            let Err(e) = super::client(config, "test".into(), false, false) else { panic!("kube uses {url}") };
            assert_eq!(crate::k8s::short_error(&e), format!("configured proxy {shown} uses an unsupported protocol"));
            for said in [crate::k8s::error_chain(&e), format!("{e:#}"), format!("{e:?}"), format!("{e:#?}")] {
                assert!(!said.contains("secret") && !said.contains("user:"), "{said}");
                assert!(said.contains("proxy.corp:"), "{said}");
            }
        }
    }

    /// A password of digits up to an unescaped `#` reads as a port in the URL kube keeps
    /// (`user:1234`): the proxy's text, as written in the kubeconfig, tells that it was cut, and
    /// only the scheme is shown then (#889).
    #[test]
    fn a_proxy_password_cut_at_a_hash_stays_out_of_the_connect_error() {
        let url = "socks5h://user:1234#secret@proxy.corp:1080";
        let kc = Kubeconfig::from_yaml(&format!(
            "clusters: [{{name: c, cluster: {{server: https://127.0.0.1:1, proxy-url: '{url}'}}}}]\n\
             contexts: [{{name: ctx, context: {{cluster: c}}}}]\n"
        ))
        .unwrap();
        // The kubeconfig's text comes before `HTTPS_PROXY`.
        let text = super::proxy_text(&kc, "ctx");
        assert_eq!(text.as_deref(), Some(url));
        let connect = |cut: bool| {
            let mut config = kube::Config::new("https://127.0.0.1:1".parse().unwrap());
            config.proxy_url = Some(url.parse().unwrap());
            let Err(e) = super::client(config, "test".into(), false, cut) else { panic!("kube uses {url}") };
            e
        };
        // From the URL kube keeps alone, the password reads as a port.
        assert!(format!("{:#}", connect(false)).contains("user:1234"));
        let e = connect(text.is_some_and(|t| t.contains('#')));
        assert_eq!(crate::k8s::short_error(&e), "configured proxy socks5h://… uses an unsupported protocol");
        for said in [crate::k8s::error_chain(&e), format!("{e:#}"), format!("{e:?}"), format!("{e:#?}")] {
            assert!(!said.contains("1234") && !said.contains("user"), "{said}");
        }
    }

    #[test]
    fn exec_command_of_a_context() {
        let kc = Kubeconfig::from_yaml(
            "
contexts:
- name: oidc
  context: {cluster: c, user: me}
- name: token
  context: {cluster: c, user: tok}
users:
- name: me
  user:
    exec: {apiVersion: client.authentication.k8s.io/v1beta1, command: kubelogin, args: [get-token, --x=1]}
- name: tok
  user: {token: abc}
",
        )
        .unwrap();
        assert_eq!(exec_command(&kc, "oidc").as_deref(), Some("kubelogin get-token --x=1"));
        assert_eq!(exec_command(&kc, "token"), None);
        assert_eq!(exec_command(&kc, "missing"), None);
    }

    #[test]
    fn a_context_points_to_its_clusters_server() {
        let kc = Kubeconfig::from_yaml(
            "
clusters:
- name: c
  cluster: {server: 'https://api.example:443'}
contexts:
- name: oidc
  context: {cluster: c, user: me}
- name: lost
  context: {cluster: gone, user: tok}
users:
- name: me
  user:
    exec: {apiVersion: client.authentication.k8s.io/v1beta1, command: kubelogin, args: [get-token]}
- name: tok
  user: {token: abc}
",
        )
        .unwrap();
        let targets = super::targets_of(&kc);
        assert_eq!(targets[0].server.as_deref(), Some("https://api.example:443"));
        assert_eq!(targets[0].exec.as_deref(), Some("kubelogin get-token"));
        assert_eq!(
            (targets[1].cluster.as_str(), targets[1].server.as_deref(), targets[1].exec.as_deref()),
            ("gone", None, None)
        );
    }

    #[test]
    fn fingerprint_follows_what_a_context_points_to() {
        let kc = |server: &str, token: &str| {
            Kubeconfig::from_yaml(&format!(
                "
clusters:
- {{name: a, cluster: {{server: '{server}'}}}}
- {{name: b, cluster: {{server: 'https://b'}}}}
contexts:
- {{name: staging, context: {{cluster: a, user: me}}}}
- {{name: other, context: {{cluster: b, user: me}}}}
users:
- {{name: me, user: {{token: {token}}}}}
"
            ))
            .unwrap()
        };
        let fp = |kc: &Kubeconfig, ctx: &str| super::fingerprint(kc, ctx);
        let base = kc("https://a", "t1");
        assert!(fp(&base, "staging").is_some());
        assert_eq!(fp(&base, "staging"), fp(&kc("https://a", "t1"), "staging"));
        assert_ne!(fp(&base, "staging"), fp(&base, "other"));
        assert_ne!(fp(&base, "staging"), fp(&kc("https://b", "t1"), "staging"));
        assert_ne!(fp(&base, "staging"), fp(&kc("https://a", "t2"), "staging"));
        assert_eq!(fp(&base, "missing"), None);
    }
}

/// Which file a context name belongs to must not move while that file still has it: the
/// name carries the context's protection, color and settings, and moving it pointed them at
/// another cluster (three audit rounds in a row).
#[cfg(test)]
mod name_properties {
    use super::name_owners;
    use proptest::prelude::*;

    type Files = Vec<(String, Option<Vec<String>>)>;

    fn files() -> impl Strategy<Value = Files> {
        let names = prop::sample::subsequence(vec!["prod", "dev", "kind"], 0..=3)
            .prop_map(|v| v.into_iter().map(String::from).collect::<Vec<_>>());
        // Some files fail to read (None): they keep what they had.
        let file = prop::option::weighted(0.85, names);
        prop::collection::vec(file, 1..5)
            .prop_map(|v| v.into_iter().enumerate().map(|(i, n)| (format!("/kube/f{i}"), n)).collect())
    }

    proptest! {
        #[test]
        fn a_name_stays_with_its_file(
            first in files(),
            order in any::<prop::sample::Index>(),
            extra in prop::option::of(prop::sample::subsequence(vec!["prod", "dev", "kind"], 0..=3)),
            shrink in prop::option::of((any::<prop::sample::Index>(), 0..3usize)),
        ) {
            let mut kept = Default::default();
            let before = name_owners(&mut kept, &first, |_| true, |_| true);
            // The same files read again in another order, maybe with a new one.
            let mut again = first.clone();
            let i = order.index(again.len());
            again.rotate_left(i);
            if let Some(names) = extra {
                again.push(("/kube/new".into(), Some(names.into_iter().map(String::from).collect())));
            }
            // Maybe one caught half-written, with fewer names or none.
            let shrunk = shrink.map(|(file, keep)| {
                let ix = file.index(again.len());
                if let Some(names) = again[ix].1.as_mut() {
                    names.truncate(keep);
                }
                again[ix].0.clone()
            });
            let after = name_owners(&mut kept, &again, |_| true, |f| shrunk.as_deref() != Some(f));
            for (name, owner) in &before {
                let still_has = first.iter().any(|(f, n)| f == owner && n.as_ref().is_none_or(|n| n.contains(name)));
                if !still_has {
                    continue;
                }
                prop_assert_eq!(kept.get(name), Some(owner), "{} given up", name);
                if again.iter().any(|(_, n)| n.as_ref().is_some_and(|n| n.contains(name))) {
                    prop_assert_eq!(after.get(name), Some(owner), "{} moved", name);
                }
            }
        }
    }
}

/// The whole merge over a sequence of kubeconfig edits: files reordered, shrinking, emptied,
/// corrupt or deleted between reads, with clashing cluster and user names. A bare context
/// name carries its protection, color and settings, so it must point at the file that owns
/// it, and never move while that file still has it (kubeconfig naming broke in every audit
/// round). A corrupt file is said by where it breaks, never by what it holds (#860).
#[cfg(test)]
mod merge_properties {
    use super::{merge_owned, name_owners};
    use kube::config::{AuthInfo, Cluster, Context, Kubeconfig, NamedAuthInfo, NamedCluster, NamedContext};
    use proptest::prelude::*;
    use std::collections::{BTreeMap, HashMap};

    const NAMES: [&str; 3] = ["prod", "dev", "kind"];
    const FILES: usize = 3;

    #[derive(Clone, Debug)]
    enum File {
        Read(Vec<&'static str>),
        Corrupt,
        Deleted,
    }

    fn file() -> impl Strategy<Value = File> {
        prop_oneof![
            6 => prop::sample::subsequence(NAMES.to_vec(), 0..=3).prop_map(File::Read),
            1 => Just(File::Corrupt),
            1 => Just(File::Deleted),
        ]
    }

    /// Every file's state at one read, and the order the files come in.
    fn steps() -> impl Strategy<Value = Vec<(Vec<File>, usize)>> {
        prop::collection::vec((prop::collection::vec(file(), FILES), 0..FILES), 1..6)
    }

    /// File `i`: each context on a cluster and user of the same names in every file (they
    /// clash), told apart by the server and the user name.
    fn kubeconfig(i: usize, names: &[&str]) -> Kubeconfig {
        Kubeconfig {
            clusters: names
                .iter()
                .map(|n| NamedCluster {
                    name: n.to_string(),
                    cluster: Some(Cluster { server: Some(format!("https://f{i}.{n}")), ..Cluster::default() }),
                    ..NamedCluster::default()
                })
                .collect(),
            auth_infos: vec![NamedAuthInfo {
                name: "admin".into(),
                auth_info: Some(AuthInfo { username: Some(format!("f{i}")), ..AuthInfo::default() }),
                ..NamedAuthInfo::default()
            }],
            contexts: names
                .iter()
                .map(|n| NamedContext {
                    name: n.to_string(),
                    context: Some(Context { cluster: n.to_string(), user: Some("admin".into()), ..Context::default() }),
                    ..NamedContext::default()
                })
                .collect(),
            current_context: names.first().map(ToString::to_string),
            ..Kubeconfig::default()
        }
    }

    /// In each place a kubeconfig keeps a secret, and never in what is said of it.
    const SECRET: &str = "XSECRETX";

    /// A kubeconfig with `SECRET` in a CA, a token, a client key, a password, and a login
    /// command's arguments and environment.
    fn with_secrets() -> String {
        format!(
            "apiVersion: v1
kind: Config
clusters:
- name: c
  cluster:
    server: https://10.0.0.1
    certificate-authority-data: {SECRET}ca
users:
- name: u
  user:
    token: {SECRET}token
    client-key-data: {SECRET}key
    username: admin
    password: {SECRET}password
- name: oidc
  user:
    exec:
      apiVersion: client.authentication.k8s.io/v1
      command: kubelogin
      args: [get-token, --oidc-client-secret={SECRET}arg]
      env: [{{name: CLIENT_SECRET, value: {SECRET}env}}]
contexts:
- name: x
  context: {{cluster: c, user: u}}
current-context: x
"
        )
    }

    /// A change that may break a file: at a byte of it, some bytes taken out and a piece put
    /// in (text, a YAML indicator, a line break or a tab, the secret as a key, a tag or an
    /// anchor, so that the parser's error is about it).
    fn edit() -> impl Strategy<Value = (prop::sample::Index, usize, String)> {
        let indicator = prop::sample::select(vec![
            "\n", "\t", "  ", ": ", "- ", "? ", "[", "]", "{", "}", ",", "\"", "'", "@", "`", "&a ", "*a", "!", "|",
            ">", "#", "%", "---\n",
        ]);
        let secret = prop::sample::select(vec!["\n$\n", "$: ", "- $", "&$ ", "*$", "!$ ", "\"$", "$: $: "]);
        let secret = secret.prop_map(|p| p.replace('$', SECRET));
        let piece = prop_oneof!["[ -~]{0,6}", indicator.prop_map(String::from), secret];
        (any::<prop::sample::Index>(), 0..8usize, piece)
    }

    /// Where a merged context points: (server, user name).
    fn target(kc: &Kubeconfig, context: &str) -> Option<(String, String)> {
        let ctx = kc.contexts.iter().find(|c| c.name == context)?.context.as_ref()?;
        let server = kc.clusters.iter().find(|c| c.name == ctx.cluster)?.cluster.as_ref()?.server.clone()?;
        let user =
            kc.auth_infos.iter().find(|u| Some(&u.name) == ctx.user.as_ref())?.auth_info.as_ref()?.username.clone()?;
        Some((server, user))
    }

    proptest! {
        #[test]
        fn every_context_keeps_its_cluster_and_a_name_stays_with_its_file(steps in steps()) {
            let mut kept = BTreeMap::new();
            // Which file had each bare name at the last read.
            let mut bare: HashMap<&str, usize> = HashMap::new();
            for (states, first) in steps {
                let order: Vec<usize> = (0..FILES).map(|k| (first + k) % FILES).collect();
                let listed: Vec<(String, Option<Vec<String>>)> = order
                    .iter()
                    .filter_map(|&i| match &states[i] {
                        File::Read(n) => Some((format!("/kube/f{i}"), Some(n.iter().map(ToString::to_string).collect()))),
                        File::Corrupt => Some((format!("/kube/f{i}"), None)),
                        File::Deleted => None,
                    })
                    .collect();
                let exists = |f: &str| listed.iter().any(|(p, _)| p == f);
                let owners = name_owners(&mut kept, &listed, exists, |_| true);
                let read: Vec<(String, String, Kubeconfig)> = order
                    .iter()
                    .filter_map(|&i| match &states[i] {
                        File::Read(n) => Some((format!("f{i}"), format!("/kube/f{i}"), kubeconfig(i, n))),
                        _ => None,
                    })
                    .collect();
                let Some(merged) = merge_owned(read, &owners, &mut kept) else {
                    prop_assert!(states.iter().all(|s| !matches!(s, File::Read(_))));
                    bare.clear();
                    continue;
                };
                // Names are unique, and the current context is one of them.
                let mut names: Vec<&str> = merged.contexts.iter().map(|c| c.name.as_str()).collect();
                names.sort_unstable();
                let before = names.len();
                names.dedup();
                prop_assert_eq!(names.len(), before, "duplicate context names: {:?}", merged.contexts);
                if let Some(current) = &merged.current_context {
                    prop_assert!(names.contains(&current.as_str()), "current context {} is gone", current);
                }
                // Every context of every file read is there once, on its own cluster and user.
                for (i, state) in states.iter().enumerate() {
                    let File::Read(own) = state else { continue };
                    for n in own {
                        let want = (format!("https://f{i}.{n}"), format!("f{i}"));
                        let found = merged.contexts.iter().filter(|c| target(&merged, &c.name).as_ref() == Some(&want)).count();
                        prop_assert_eq!(found, 1, "{} of f{} found {} times", n, i, found);
                    }
                }
                let mut now: HashMap<&str, usize> = HashMap::new();
                for n in NAMES {
                    let Some((server, _)) = target(&merged, n) else { continue };
                    let from = (0..FILES).find(|i| server == format!("https://f{i}.{n}")).unwrap();
                    // A bare name is its owner's.
                    prop_assert_eq!(owners.get(n), Some(&format!("/kube/f{from}")));
                    now.insert(n, from);
                }
                // The file that had a bare name keeps it while it still has it, and while it
                // can't be read nobody else takes it.
                for (n, &was) in &bare {
                    match &states[was] {
                        File::Read(own) if own.contains(n) => prop_assert_eq!(now.get(n), Some(&was), "{} moved", n),
                        File::Corrupt => prop_assert!(!now.contains_key(n), "{} went to f{:?} while f{} was unreadable", n, now.get(n), was),
                        _ => {}
                    }
                }
                // A name held by an unreadable file stays held: carry it over.
                for (n, &was) in &bare {
                    if matches!(states[was], File::Corrupt) {
                        now.insert(n, was);
                    }
                }
                bare = now;
            }
        }

        /// Whatever a corrupt kubeconfig holds around where it breaks, what is said of it names
        /// the file and never quotes it: it reaches pages, agents and the audit log. So for
        /// pasted text.
        #[test]
        fn a_corrupt_file_never_shows_what_it_holds(
            edits in prop::collection::vec(edit(), 1..6),
            cut_short in prop::option::of(any::<prop::sample::Index>()),
        ) {
            let mut text = with_secrets();
            // All ASCII: any byte is a character boundary.
            for (at, cut, piece) in edits {
                let at = at.index(text.len() + 1);
                text.replace_range(at..(at + cut).min(text.len()), &piece);
            }
            if let Some(end) = cut_short {
                text.truncate(end.index(text.len() + 1));
            }
            let path = std::env::temp_dir().join(format!("recouvrance-test-{}.yaml", uuid::Uuid::new_v4().simple()));
            std::fs::write(&path, &text).unwrap();
            let read = super::read_kubeconfig_file(&path);
            std::fs::remove_file(&path).unwrap();
            // `{:#}` says the whole chain.
            if let Err(e) = read {
                let file = crate::paths::tilde(&path);
                prop_assert!(e.to_string().starts_with(&format!("{file}: ")), "{e}");
                prop_assert!(!format!("{e:#}").contains(SECRET), "{e:#}\nfrom\n{text}");
            }
            if let Err(e) = super::parse_kubeconfig(&text) {
                prop_assert!(!format!("{e:#}").contains(SECRET), "{e:#}\nfrom\n{text}");
            }
        }

        /// Whatever bytes a kubeconfig file holds after a byte order mark or none, it is read or
        /// said not to read, naming the file: never a panic, which crashed the app on a UTF-16
        /// mark followed by an odd number of bytes (#890).
        #[test]
        fn any_file_reads_or_fails_without_a_crash(
            mark in prop_oneof![
                Just(&[0u8; 0][..]),
                Just(&[0xFF, 0xFE][..]),
                Just(&[0xFE, 0xFF][..]),
                Just(&[0xEF, 0xBB, 0xBF][..]),
            ],
            body in prop::collection::vec(any::<u8>(), 0..64),
        ) {
            /// The test's folder, removed when the case ends, failed or not.
            struct Folder(std::path::PathBuf);
            impl Drop for Folder {
                fn drop(&mut self) {
                    let _ = std::fs::remove_dir_all(&self.0);
                }
            }
            let dir = Folder(crate::mcp::test_dir());
            let path = dir.0.join("config");
            std::fs::write(&path, [mark, &body[..]].concat()).unwrap();
            if let Err(e) = super::read_kubeconfig_file(&path) {
                let file = crate::paths::tilde(&path);
                prop_assert!(e.to_string().starts_with(&format!("{file}: ")), "{e}");
            }
        }
    }
}
