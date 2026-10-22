//! Kubeconfig files and contexts: merging, watching, connecting, private copies for tools.

#[allow(unused_imports)]
use super::*;

#[derive(Clone)]
pub struct Contexts {
    pub names: Vec<String>,
    pub current: Option<String>,
    /// Contexts whose name or user suggests production or admin rights.
    pub risky: Vec<String>,
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
/// user of the same name) get `@<file name>` so both stay usable.
pub(super) fn merge_unique(kc: &mut Kubeconfig, mut more: Kubeconfig, file: &str) {
    let rename = |name: &str| format!("{name}@{file}");
    let taken_clusters: std::collections::HashSet<String> = kc.clusters.iter().map(|c| c.name.clone()).collect();
    let taken_users: std::collections::HashSet<String> = kc.auth_infos.iter().map(|c| c.name.clone()).collect();
    let taken_contexts: std::collections::HashSet<String> = kc.contexts.iter().map(|c| c.name.clone()).collect();
    for c in &mut more.clusters {
        if taken_clusters.contains(&c.name) {
            let old = c.name.clone();
            c.name = rename(&old);
            for ctx in more.contexts.iter_mut().filter_map(|x| x.context.as_mut()) {
                if ctx.cluster == old {
                    ctx.cluster = c.name.clone();
                }
            }
        }
    }
    for u in &mut more.auth_infos {
        if taken_users.contains(&u.name) {
            let old = u.name.clone();
            u.name = rename(&old);
            for ctx in more.contexts.iter_mut().filter_map(|x| x.context.as_mut()) {
                if ctx.user.as_deref() == Some(old.as_str()) {
                    ctx.user = Some(u.name.clone());
                }
            }
        }
    }
    for c in &mut more.contexts {
        if taken_contexts.contains(&c.name) {
            c.name = rename(&c.name);
        }
    }
    kc.clusters.extend(more.clusters);
    kc.auth_infos.extend(more.auth_infos);
    kc.contexts.extend(more.contexts);
    if kc.current_context.is_none() {
        kc.current_context = more.current_context;
    }
}

/// All kubeconfig files merged (see `kubeconfig_files`); unreadable files are skipped.
pub fn kubeconfig() -> Result<Kubeconfig> {
    let mut kc: Option<Kubeconfig> = None;
    let mut last_err = None;
    for path in kubeconfig_files() {
        match Kubeconfig::read_from(&path) {
            Ok(more) => match kc.as_mut() {
                None => kc = Some(more),
                Some(k) => {
                    let file =
                        path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
                    merge_unique(k, more, &file);
                }
            },
            Err(e) => last_err = Some(e),
        }
    }
    match (kc, last_err) {
        (Some(k), _) => Ok(k),
        (None, Some(e)) => Err(e).context("reading kubeconfig"),
        (None, None) => anyhow::bail!(
            "no kubeconfig found (KUBECONFIG or ~/.kube/config): add or paste one in Settings → Kubeconfig"
        ),
    }
}

/// Contexts from the kubeconfig files, cached: the files are only read again when
/// they change (checked at most every two seconds). Safe to call while rendering.
pub fn load_contexts() -> Result<Contexts> {
    type Cached = (std::time::Instant, Vec<(std::path::PathBuf, Option<std::time::SystemTime>, u64)>, Contexts);
    static CACHE: std::sync::Mutex<Option<Cached>> = std::sync::Mutex::new(None);
    let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((checked, _, contexts)) = cache.as_ref()
        && checked.elapsed() < std::time::Duration::from_secs(2)
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
    Ok(Contexts { names, current: kc.current_context, risky })
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

pub async fn connect(context: String) -> Result<Client> {
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
        Ok(kube::client::ClientBuilder::try_from(config)?.with_layer(&timeout).build())
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
    if let Some(p) = &cluster.proxy_url {
        c["proxy-url"] = p.clone().into();
    }
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
}
