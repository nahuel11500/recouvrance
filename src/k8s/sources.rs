//! Where kubeconfigs come from, as the Settings page lists them, and kubeconfigs
//! pasted into Recouvrance (kept in its own folder, private).

use anyhow::{Context as _, Result, anyhow};
use kube::config::Kubeconfig;

use super::{create_private_file, load_contexts, safe_file_part};

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum SourceKind {
    /// KUBECONFIG or ~/.kube/config; not removable here.
    Default,
    /// An extra file from settings (`kubeconfigs`).
    File,
    /// A watched folder (`kubeconfig_folders`).
    Folder,
    /// Pasted into Recouvrance; removing it deletes the file.
    Added,
}

#[derive(Clone, PartialEq, Eq, Debug)]
pub struct Source {
    pub kind: SourceKind,
    /// As entered in settings (`~/` kept), or the full path.
    pub path: String,
    pub files: usize,
    pub contexts: usize,
    /// Missing, unreadable or not a kubeconfig.
    pub error: Option<String>,
}

/// The folder for kubeconfigs pasted into Recouvrance.
pub fn added_dir() -> std::path::PathBuf {
    crate::mcp::config_dir().join("kubeconfigs")
}

/// Files of a folder that look like kubeconfigs: no hidden files, YAML-ish or no extension.
/// Oldest first, so a file added later never takes a context name an older one already has.
pub(super) fn folder_files(dir: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut found: Vec<std::path::PathBuf> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_file() && !p.file_name().is_some_and(|n| n.to_string_lossy().starts_with('.')))
        .filter(|p| {
            p.extension().is_none_or(|x| matches!(x.to_str(), Some("yaml" | "yml" | "conf" | "config" | "kubeconfig")))
        })
        .collect();
    found.sort_by_cached_key(|p| {
        let m = std::fs::metadata(p).ok();
        (m.and_then(|m| m.created().or_else(|_| m.modified()).ok()), p.clone())
    });
    found
}

/// `/home/me/x` as `~/x`, the way paths are kept in settings.
pub fn tilde(p: &std::path::Path) -> String {
    let home = std::env::var_os("HOME").map(std::path::PathBuf::from).unwrap_or_default();
    match p.strip_prefix(&home) {
        Ok(rest) if !home.as_os_str().is_empty() => format!("~/{}", rest.display()),
        _ => p.display().to_string(),
    }
}

fn count(files: &[std::path::PathBuf]) -> (usize, Option<String>) {
    let mut contexts = 0;
    let mut error = None;
    for f in files {
        match Kubeconfig::read_from(f) {
            Ok(kc) => contexts += kc.contexts.len(),
            Err(e) => error = Some(format!("{}: {}", f.file_name().unwrap_or_default().to_string_lossy(), short(&e))),
        }
    }
    (contexts, error)
}

fn short(e: &impl std::fmt::Display) -> String {
    let s = e.to_string();
    s.lines().next().unwrap_or_default().chars().take(160).collect()
}

fn file_source(kind: SourceKind, shown: String, path: &std::path::Path) -> Source {
    if !path.is_file() {
        return Source { kind, path: shown, files: 0, contexts: 0, error: Some("not found".into()) };
    }
    let (contexts, error) = count(&[path.to_path_buf()]);
    Source { kind, path: shown, files: 1, contexts, error }
}

/// Every source, in the order they are merged.
pub fn sources() -> Vec<Source> {
    let mut out = Vec::new();
    if let Some(v) = std::env::var_os("KUBECONFIG").filter(|v| !v.is_empty()) {
        for p in std::env::split_paths(&v) {
            out.push(file_source(SourceKind::Default, tilde(&p), &p));
        }
    } else {
        let p = std::path::PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".kube").join("config");
        out.push(file_source(SourceKind::Default, "~/.kube/config".into(), &p));
    }
    let st = crate::settings::get();
    for p in &st.kubeconfigs {
        out.push(file_source(SourceKind::File, p.clone(), &crate::settings::expand(p)));
    }
    for d in &st.kubeconfig_folders {
        let dir = crate::settings::expand(d);
        if !dir.is_dir() {
            out.push(Source {
                kind: SourceKind::Folder,
                path: d.clone(),
                files: 0,
                contexts: 0,
                error: Some("not found".into()),
            });
            continue;
        }
        let files = folder_files(&dir);
        let (contexts, error) = count(&files);
        out.push(Source { kind: SourceKind::Folder, path: d.clone(), files: files.len(), contexts, error });
    }
    for f in folder_files(&added_dir()) {
        out.push(file_source(SourceKind::Added, tilde(&f), &f));
    }
    out
}

/// How a path is kept in settings: absolute (a typed `team.yaml` is relative to where
/// Recouvrance was started, which changes), then with `~/`.
fn settings_path(p: &std::path::Path) -> String {
    tilde(&std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()))
}

/// Add files or folders picked or typed in Settings. Returns what was not added and why.
pub fn add_paths(paths: &[std::path::PathBuf]) -> Vec<String> {
    let mut problems = Vec::new();
    for p in paths {
        let shown = settings_path(p);
        if p.is_dir() {
            crate::settings::update(|s| {
                if !s.kubeconfig_folders.contains(&shown) {
                    s.kubeconfig_folders.push(shown.clone());
                }
            });
        } else if p.is_file() {
            if let Err(e) = Kubeconfig::read_from(p) {
                problems.push(format!("{shown}: not a kubeconfig ({})", short(&e)));
                continue;
            }
            crate::settings::update(|s| {
                if !s.kubeconfigs.contains(&shown) {
                    s.kubeconfigs.push(shown.clone());
                }
            });
        } else {
            problems.push(format!("{shown}: not found"));
        }
    }
    problems
}

/// Stop using a source. A pasted kubeconfig's file is deleted; files and folders added
/// from elsewhere are only forgotten.
pub fn remove_source(src: &Source) -> Result<()> {
    match src.kind {
        SourceKind::Default => anyhow::bail!("the default kubeconfig is set by KUBECONFIG"),
        SourceKind::File => crate::settings::update(|s| s.kubeconfigs.retain(|p| *p != src.path)),
        SourceKind::Folder => crate::settings::update(|s| s.kubeconfig_folders.retain(|p| *p != src.path)),
        SourceKind::Added => {
            let path = crate::settings::expand(&src.path);
            // Only files in our own folder.
            anyhow::ensure!(path.parent() == Some(added_dir().as_path()), "not a pasted kubeconfig");
            std::fs::remove_file(&path).with_context(|| format!("deleting {}", src.path))?;
        }
    }
    Ok(())
}

/// What a pasted kubeconfig holds, to show before saving it.
#[derive(Debug, Default)]
pub struct Pasted {
    pub contexts: Vec<String>,
    /// Context names already in use: they will show as `name@file`.
    pub taken: Vec<String>,
    /// Name proposed for the file: the current context, or the first one.
    pub suggested_name: String,
    /// Things to know before saving: commands run to log in, files it points at.
    pub warnings: Vec<String>,
}

/// Parse and check pasted text.
pub fn inspect_pasted(text: &str) -> Result<Pasted> {
    anyhow::ensure!(!text.trim().is_empty(), "paste a kubeconfig");
    let kc = Kubeconfig::from_yaml(text).map_err(|e| anyhow!("not a kubeconfig: {}", short(&e)))?;
    anyhow::ensure!(!kc.contexts.is_empty(), "no contexts in this kubeconfig");
    let mut p = Pasted {
        contexts: kc.contexts.iter().map(|c| c.name.clone()).collect(),
        suggested_name: kc.current_context.clone().unwrap_or_else(|| kc.contexts[0].name.clone()),
        ..Default::default()
    };
    let existing = load_contexts().map(|c| c.names).unwrap_or_default();
    p.taken = p.contexts.iter().filter(|c| existing.contains(c)).cloned().collect();
    for c in &kc.contexts {
        let Some(ctx) = &c.context else {
            p.warnings.push(format!("Context {} has no cluster.", c.name));
            continue;
        };
        if !kc.clusters.iter().any(|k| k.name == ctx.cluster) {
            p.warnings
                .push(format!("Context {} uses cluster {}, which is not in this kubeconfig.", c.name, ctx.cluster));
        }
        if let Some(u) = &ctx.user
            && !kc.auth_infos.iter().any(|a| a.name == *u)
        {
            p.warnings.push(format!("Context {} uses user {u}, which is not in this kubeconfig.", c.name));
        }
    }
    for k in &kc.clusters {
        let Some(c) = &k.cluster else { continue };
        if c.insecure_skip_tls_verify == Some(true) {
            p.warnings.push(format!("Cluster {} skips TLS verification.", k.name));
        }
        if let Some(f) = &c.certificate_authority {
            p.warnings.push(format!("Cluster {} reads its CA from {f}.", k.name));
        }
    }
    for a in &kc.auth_infos {
        let Some(u) = &a.auth_info else { continue };
        if let Some(exec) = &u.exec {
            let cmd = std::iter::once(exec.command.clone().unwrap_or_default())
                .chain(exec.args.clone().unwrap_or_default())
                .collect::<Vec<_>>()
                .join(" ");
            p.warnings.push(format!("User {} runs this command to log in: {cmd}", a.name));
        }
        if let Some(ap) = &u.auth_provider {
            p.warnings.push(format!("User {} logs in with the {} auth provider.", a.name, ap.name));
        }
        for f in [&u.client_certificate, &u.client_key, &u.token_file].into_iter().flatten() {
            p.warnings.push(format!("User {} reads {f}.", a.name));
        }
    }
    Ok(p)
}

/// Save a pasted kubeconfig as `<name>.yaml` in `added_dir()` (mode 0600, never
/// replacing a file: a taken name gets `-1`, `-2`…). The text is kept as pasted.
pub fn save_pasted(text: &str, name: &str) -> Result<std::path::PathBuf> {
    inspect_pasted(text)?;
    let dir = added_dir();
    let mut b = std::fs::DirBuilder::new();
    b.recursive(true);
    std::os::unix::fs::DirBuilderExt::mode(&mut b, 0o700);
    b.create(&dir)?;
    create_private_file(&dir, &pasted_base(name), "yaml", text.as_bytes())
}

/// The file name (without `.yaml`) a pasted kubeconfig named `name` would be saved
/// under now, as `save_pasted` picks it: its contexts get `@` and this when renamed.
pub fn pasted_stem(name: &str) -> String {
    free_stem(&added_dir(), &pasted_base(name))
}

fn pasted_base(name: &str) -> String {
    let stem = safe_file_part(name.trim());
    if stem.trim_matches(['.', '_']).is_empty() { "kubeconfig".to_string() } else { stem }
}

/// `stem`, or `stem-1`, `stem-2`… when `<stem>.yaml` is taken in `dir` (as `create_private_file`).
fn free_stem(dir: &std::path::Path, stem: &str) -> String {
    (0..1000)
        .map(|i| if i == 0 { stem.to_string() } else { format!("{stem}-{i}") })
        .find(|s| !dir.join(format!("{s}.yaml")).exists())
        .unwrap_or_else(|| stem.to_string())
}

#[cfg(test)]
mod tests {
    const KC: &str = "apiVersion: v1\nkind: Config\nclusters: [{name: c, cluster: {server: 'https://10.0.0.1', insecure-skip-tls-verify: true}}]\n\
                      users: [{name: u, user: {exec: {apiVersion: client.authentication.k8s.io/v1, command: aws, args: [eks, get-token]}}}]\n\
                      contexts: [{name: lab, context: {cluster: c, user: u}}, {name: lab2, context: {cluster: gone, user: u}}]\n\
                      current-context: lab2\n";

    #[test]
    fn pasted_kubeconfig_is_checked() {
        let p = super::inspect_pasted(KC).unwrap();
        assert_eq!(p.contexts, ["lab", "lab2"]);
        assert_eq!(p.suggested_name, "lab2");
        let w = p.warnings.join("\n");
        assert!(w.contains("aws eks get-token"), "{w}");
        assert!(w.contains("skips TLS"), "{w}");
        assert!(w.contains("cluster gone"), "{w}");
        assert!(super::inspect_pasted("").is_err());
        assert!(super::inspect_pasted("kind: Config\ncontexts: []\n").is_err());
        assert!(super::inspect_pasted("{not yaml").is_err());
    }

    #[test]
    fn pasted_file_names() {
        assert_eq!(super::pasted_base(""), "kubeconfig");
        assert_eq!(super::pasted_base(" .. "), "kubeconfig");
        assert_eq!(super::pasted_base("arn:aws:eks:x/prod"), "arn_aws_eks_x_prod");
        let dir = std::env::temp_dir().join(format!("recouvrance-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        assert_eq!(super::free_stem(&dir, "kubeconfig"), "kubeconfig");
        let a = super::create_private_file(&dir, "kubeconfig", "yaml", b"a").unwrap();
        assert_eq!(super::free_stem(&dir, "kubeconfig"), "kubeconfig-1");
        let b = super::create_private_file(&dir, "kubeconfig", "yaml", b"b").unwrap();
        assert_eq!(b.file_stem().unwrap(), "kubeconfig-1");
        assert_eq!(super::free_stem(&dir, "kubeconfig"), "kubeconfig-2");
        assert_eq!(a.file_stem().unwrap(), "kubeconfig");
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn folder_files_oldest_first() {
        let dir = std::env::temp_dir().join(format!("recouvrance-test-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("zeta.yaml"), "a").unwrap();
        // File times can be a few milliseconds coarse.
        std::thread::sleep(std::time::Duration::from_millis(50));
        std::fs::write(dir.join("alpha.yaml"), "b").unwrap();
        std::fs::write(dir.join(".hidden.yaml"), "c").unwrap();
        std::fs::write(dir.join("notes.txt"), "d").unwrap();
        let names: Vec<String> =
            super::folder_files(&dir).iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, ["zeta.yaml", "alpha.yaml"]);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn tilde_shortens_home() {
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(super::tilde(&home.join(".kube/a")), "~/.kube/a");
        assert_eq!(super::tilde(std::path::Path::new("/etc/k")), "/etc/k");
    }

    #[test]
    fn typed_paths_are_kept_absolute() {
        let cwd = std::env::current_dir().unwrap();
        assert_eq!(super::settings_path(std::path::Path::new("team.yaml")), super::tilde(&cwd.join("team.yaml")));
        assert_eq!(super::settings_path(std::path::Path::new("/etc/k")), "/etc/k");
        let home = std::path::PathBuf::from(std::env::var_os("HOME").unwrap());
        assert_eq!(super::settings_path(&home.join(".kube/a")), "~/.kube/a");
    }
}
