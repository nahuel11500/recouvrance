//! Kubeconfig files and contexts: merging, watching, connecting, private copies for tools.

#[allow(unused_imports)]
use super::*;

#[derive(Clone)]
pub struct Contexts {
    pub names: Vec<String>,
    pub current: Option<String>,
    /// Contexts whose name or user suggests production or admin rights.
    pub risky: Vec<String>,
    /// What each context points to (see `fingerprint`), to notice it changed.
    pub fingerprints: HashMap<String, u64>,
}

/// Every kubeconfig file in use: KUBECONFIG (or ~/.kube/config), the extra files
/// from settings, each file in the watched folders and the kubeconfigs pasted in
/// Settings. Existing files only, no duplicates.
pub fn kubeconfig_files() -> Vec<std::path::PathBuf> {
    let mut files: Vec<std::path::PathBuf> = match std::env::var_os("KUBECONFIG").filter(|v| !v.is_empty()) {
        Some(v) => std::env::split_paths(&v).collect(),
        None => {
            vec![std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".kube").join("config")]
        }
    };
    let st = crate::settings::get();
    files.extend(st.kubeconfigs.iter().map(|p| crate::settings::expand(p)));
    for dir in &st.kubeconfig_folders {
        files.extend(folder_files(&crate::settings::expand(dir)));
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
                    ctx.cluster = c.name.clone();
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
    let mut files = Vec::new();
    let mut last_err = None;
    for path in kubeconfig_files() {
        let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
        let key = tilde(&path.canonicalize().unwrap_or_else(|_| path.clone()));
        match Kubeconfig::read_from(&path) {
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
    let now = name_owners(&mut owners, &names, |f| crate::settings::expand(f).exists());
    if owners != kept {
        crate::settings::update(|s| s.context_files = owners);
    }
    let read = files.into_iter().filter_map(|(stem, key, kc)| Some((stem, key, kc?))).collect();
    match (merge_owned(read, &now), last_err) {
        (Some(k), _) => Ok(k),
        (None, Some(e)) => Err(e).context("reading kubeconfig"),
        (None, None) => anyhow::bail!(
            "no kubeconfig found (KUBECONFIG or ~/.kube/config): add or paste one in Settings → Kubeconfig"
        ),
    }
}

/// The file that has each context name in this merge, given the files read now (by
/// path, with their context names, or `None` when unreadable) and `kept`, the files
/// that had each name before. `kept` is brought up to date: a file deleted, or read
/// without the name, gives it up; one that failed to read keeps it; a new name goes to
/// the first file that has it. A name kept for a file not merged now (another
/// KUBECONFIG) goes to the first file here that has it, without changing `kept`.
fn name_owners(
    kept: &mut std::collections::BTreeMap<String, String>,
    files: &[(String, Option<Vec<String>>)],
    exists: impl Fn(&str) -> bool,
) -> HashMap<String, String> {
    let read: HashMap<&str, Option<&Vec<String>>> = files.iter().map(|(f, n)| (f.as_str(), n.as_ref())).collect();
    kept.retain(|name, file| match read.get(file.as_str()) {
        Some(Some(names)) => names.contains(name),
        Some(None) => true,
        None => exists(file.as_str()),
    });
    let mut now = HashMap::new();
    for (file, names) in files {
        for name in names.iter().flatten() {
            let owner = kept.entry(name.clone()).or_insert_with(|| file.clone());
            let owner = if read.contains_key(owner.as_str()) { owner.clone() } else { file.clone() };
            now.entry(name.clone()).or_insert(owner);
        }
    }
    now
}

/// Merge files (file name, path, contents) in order. A context whose name `owners`
/// gives to another file is renamed `name@file` first; clusters and users that clash
/// are renamed as they are merged (see `merge_unique`).
fn merge_owned(files: Vec<(String, String, Kubeconfig)>, owners: &HashMap<String, String>) -> Option<Kubeconfig> {
    let mut taken: std::collections::HashSet<String> =
        files.iter().flat_map(|(_, _, kc)| kc.contexts.iter().map(|c| c.name.clone())).collect();
    let mut merged: Option<Kubeconfig> = None;
    for (stem, key, mut more) in files {
        for c in &mut more.contexts {
            if owners.get(&c.name).is_some_and(|owner| *owner != key) {
                let new = free_name(&c.name, &stem, &mut taken);
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
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((checked, _, contexts)) = cache.as_ref()
        && checked.elapsed() < max_age
    {
        return Ok(contexts.clone());
    }
    let signature = kubeconfig_signature();
    if let Some((checked, sig, contexts)) = cache.as_mut()
        && *sig == signature
    {
        *checked = std::time::Instant::now();
        return Ok(contexts.clone());
    }
    let contexts = read_contexts()?;
    *cache = Some((std::time::Instant::now(), signature, contexts.clone()));
    Ok(contexts)
}

fn read_contexts() -> Result<Contexts> {
    let kc = kubeconfig()?;
    let mut names: Vec<String> = kc.contexts.iter().map(|c| c.name.clone()).collect();
    names.sort();
    let risky = risky_contexts(&kc);
    let fingerprints = kc.contexts.iter().filter_map(|c| Some((c.name.clone(), fingerprint(&kc, &c.name)?))).collect();
    Ok(Contexts { names, current: kc.current_context, risky, fingerprints })
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
            let cluster = ctx.map(|x| x.cluster.as_str()).unwrap_or_default();
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
        let opts = KubeConfigOptions { context: Some(context.clone()), ..Default::default() };
        let config =
            Config::from_custom_kubeconfig(kc, &opts).await.with_context(|| format!("loading context {context}"))?;
        // kube sets no read timeout: on a connection that died without a word (sleep, VPN,
        // NAT), a request would wait forever. This bounds the wait for the response headers;
        // watches, logs and shells stream after that and are not cut.
        let timeout = tower::timeout::TimeoutLayer::new(REQUEST_TIMEOUT);
        // Outside the timeout, so a request it cuts is counted as failed.
        let meter = meter::MeterLayer { context: context.clone() };
        // The exec plugin runs as a blocking process here (a browser login can take minutes):
        // off the shared runtime's few threads, so other clusters keep streaming meanwhile.
        let builder = tokio::task::spawn_blocking(move || kube::client::ClientBuilder::try_from(config))
            .await
            .context("building the client")??;
        let builder = builder.with_layer(&timeout).with_layer(&meter);
        Ok(if gated { builder.with_layer(&gate::WriteGateLayer).build() } else { builder.build() })
    })
    .await
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
    LOCKS.lock().unwrap_or_else(|e| e.into_inner()).entry(cmd).or_default().clone()
}

/// A private kubeconfig file, removed when this is dropped: keep it alive as long
/// as the process using it (dropping the task that owns it removes it too).
pub struct PrivateKubeconfig(std::path::PathBuf);

impl PrivateKubeconfig {
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
}

impl Drop for PrivateKubeconfig {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}

/// A copy of the merged kubeconfig with `context` (and `namespace`) selected,
/// readable only by the user, for tools run on this machine (shell, helm).
/// Each call gets its own file, so concurrent uses never share or overwrite one.
pub fn private_kubeconfig(context: &str, namespace: Option<&str>, purpose: &str) -> Result<PrivateKubeconfig> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let mut kc = kubeconfig()?;
    kc.current_context = Some(context.to_string());
    // Only this context, its cluster and its user: a plugin or helm never sees other clusters' credentials.
    kc.contexts.retain(|c| c.name == context);
    let (cluster, user) = kc
        .contexts
        .first()
        .and_then(|c| c.context.as_ref())
        .map(|c| (c.cluster.clone(), c.user.clone()))
        .unwrap_or_default();
    kc.clusters.retain(|c| c.name == cluster);
    kc.auth_infos.retain(|a| Some(&a.name) == user.as_ref());
    if let Some(ns) = namespace {
        for c in kc.contexts.iter_mut().filter(|c| c.name == context) {
            if let Some(ctx) = c.context.as_mut() {
                ctx.namespace = Some(ns.to_string());
            }
        }
    }
    let dir = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from).unwrap_or_else(std::env::temp_dir);
    let path = dir.join(format!("recouvrance-{}-{purpose}-{}.yaml", std::process::id(), uuid::Uuid::new_v4().simple()));
    // create_new: never follow or reuse a file someone else put there.
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path)?;
    let guard = PrivateKubeconfig(path);
    f.write_all(crate::yaml::to_string(&kc)?.as_bytes())?;
    Ok(guard)
}

/// Longest ServiceAccount token lifetime handed out, in hours.
pub const MAX_TOKEN_HOURS: i64 = 24;

/// A piece of a file name: anything but letters, digits, `.`, `_` and `-` becomes `_`
/// (EKS context names hold `:` and `/`; `../` must not leave the folder).
pub fn safe_file_part(s: &str) -> String {
    s.chars().map(|c| if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') { c } else { '_' }).collect()
}

/// Create `<stem>.<ext>` in `dir`, readable only by the user, never overwriting: an
/// existing name gets `-1`, `-2`, … appended.
pub fn create_private_file(dir: &std::path::Path, stem: &str, ext: &str, bytes: &[u8]) -> Result<std::path::PathBuf> {
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    for i in 0..1000 {
        let name = if i == 0 { format!("{stem}.{ext}") } else { format!("{stem}-{i}.{ext}") };
        let path = dir.join(name);
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path) {
            Ok(mut f) => {
                f.write_all(bytes)?;
                return Ok(path);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(e.into()),
        }
    }
    anyhow::bail!("too many files named {stem}.{ext} in {}", dir.display())
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
    let dir = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join("Downloads");
    std::fs::create_dir_all(&dir)?;
    let stem = format!("kubeconfig-{}", safe_file_part(&name));
    create_private_file(&dir, &stem, "yaml", crate::yaml::to_string(&config)?.as_bytes())
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
        let dir = std::env::temp_dir().join(format!("recouvrance-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let a = super::create_private_file(&dir, "k", "yaml", b"a").unwrap();
        let b = super::create_private_file(&dir, "k", "yaml", b"b").unwrap();
        assert_eq!(a.file_name().unwrap(), "k.yaml");
        assert_eq!(b.file_name().unwrap(), "k-1.yaml");
        assert_eq!(std::fs::read(&a).unwrap(), b"a");
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(&b).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(dir).unwrap();
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
        (file.to_string(), Some(names.iter().map(|s| s.to_string()).collect()))
    }

    #[test]
    fn a_name_stays_with_the_file_that_had_it_first() {
        let mut kept = std::collections::BTreeMap::new();
        let on_disk = |_: &str| true;
        // a.yaml was there first.
        let now = super::name_owners(&mut kept, &[read("a", &["prod"])], on_disk);
        assert_eq!(now["prod"], "a");
        // b.yaml comes, then the files are read in another order (a saved again, new birth time).
        super::name_owners(&mut kept, &[read("a", &["prod"]), read("b", &["prod"])], on_disk);
        let now = super::name_owners(&mut kept, &[read("b", &["prod"]), read("a", &["prod"])], on_disk);
        assert_eq!(now["prod"], "a");
        // a briefly unreadable: b doesn't take the name.
        let now = super::name_owners(&mut kept, &[(String::from("a"), None), read("b", &["prod"])], on_disk);
        assert_eq!(now["prod"], "a");
        assert_eq!(kept["prod"], "a");
        // A name added to an earlier file (the default kubeconfig) doesn't take it either.
        let now = super::name_owners(&mut kept, &[read("c", &["prod"]), read("a", &["prod"])], on_disk);
        assert_eq!(now["prod"], "a");
        // Read without the name, a gives it up; the next file that has it keeps it.
        let now = super::name_owners(&mut kept, &[read("a", &[]), read("b", &["prod"])], on_disk);
        assert_eq!(now["prod"], "b");
        let now = super::name_owners(&mut kept, &[read("a", &["prod"]), read("b", &["prod"])], on_disk);
        assert_eq!(now["prod"], "b");
        // A file not read now (another KUBECONFIG) keeps its names for later.
        let now = super::name_owners(&mut kept, &[read("c", &["prod"])], on_disk);
        assert_eq!(now["prod"], "c");
        assert_eq!(kept["prod"], "b");
        // Deleted, it gives them up.
        let now = super::name_owners(&mut kept, &[read("c", &["prod"])], |_| false);
        assert_eq!(now["prod"], "c");
        assert_eq!(kept["prod"], "c");
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
        let kc = super::merge_owned(files, &owners).unwrap();
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
        ) {
            let mut kept = Default::default();
            let before = name_owners(&mut kept, &first, |_| true);
            // The same files read again in another order, maybe with a new one.
            let mut again = first.clone();
            let i = order.index(again.len());
            again.rotate_left(i);
            if let Some(names) = extra {
                again.push(("/kube/new".into(), Some(names.into_iter().map(String::from).collect())));
            }
            let after = name_owners(&mut kept, &again, |_| true);
            for (name, owner) in &before {
                let still_has = first.iter().any(|(f, n)| f == owner && n.as_ref().is_none_or(|n| n.contains(name)));
                if still_has {
                    prop_assert_eq!(after.get(name), Some(owner), "{} moved", name);
                }
            }
        }
    }
}
