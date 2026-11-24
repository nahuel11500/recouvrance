//! Reading a dashboard: the Application, then its tree, its diff and its commits.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex, PoisonError};

use serde_json::Value;

use super::super::Hub;
use super::super::server::GitDiff;
use super::{Commit, Deployed, MAX_COMMITS, Read, commit_of, deployed, managed_path, revision_path, tree_path};

/// Commits read, by server, Application, source and revision: a revision's never changes.
type Commits = HashMap<(String, String, String, Option<usize>, String), Commit>;
static COMMITS: LazyLock<Mutex<Commits>> = LazyLock::new(Default::default);

/// The tree, the diff and the commits of `app` (named `namespace/name`), with `get` (a GET of
/// a path of Argo CD's API); `diff_of` turns the managed resources into Argo CD's diff.
/// `server` names the Argo CD for the commits kept.
pub(in crate::k8s::argo) async fn extras<F, Fut>(
    server: &str,
    namespace: &str,
    name: &str,
    app: Value,
    get: F,
    diff_of: impl Fn(&Value) -> GitDiff,
) -> Read
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    let (tree, managed) = futures::join!(get(tree_path(namespace, name)), get(managed_path(namespace, name)));
    let (deployed, version) = deployed(&app);
    let mut commits = Vec::with_capacity(deployed.len());
    for d in &deployed {
        commits.push(commit(server, namespace, name, d, version, &get).await);
    }
    Read { app, tree: Some(tree), diff: Some(managed.map(|m| diff_of(&m))), commits, note: None }
}

/// The commit `d` deploys, kept once read; None for a chart's version (no commit).
async fn commit<F, Fut>(
    server: &str,
    namespace: &str,
    name: &str,
    d: &Deployed,
    version: Option<i64>,
    get: &F,
) -> Option<Result<Commit, String>>
where
    F: Fn(String) -> Fut,
    Fut: Future<Output = Result<Value, String>>,
{
    if d.chart.is_some() || d.revision.is_empty() {
        return None;
    }
    let key = (server.to_string(), namespace.to_string(), name.to_string(), d.index, d.revision.clone());
    let kept = COMMITS.lock().unwrap_or_else(PoisonError::into_inner).get(&key).cloned();
    if let Some(c) = kept {
        return Some(Ok(c));
    }
    let read = get(revision_path(namespace, name, &d.revision, d.index, version)).await.map(|v| commit_of(&v));
    if let Ok(c) = &read {
        let mut all = COMMITS.lock().unwrap_or_else(PoisonError::into_inner);
        if all.len() >= MAX_COMMITS {
            all.clear();
        }
        all.insert(key, c.clone());
    }
    Some(read)
}

/// The dashboard of the Application `namespace/name` read on its hub: the Application from
/// the hub; its tree, diff and commits from the Argo CD server set for the hub's context in
/// Settings, when one is (else said why not).
pub async fn read_hub(hub: Hub, namespace: String, name: String) -> Result<Read, String> {
    let read = crate::k8s::run_abortable(async move {
        let app = match super::super::get_application(&hub, &namespace, &name).await {
            Ok(app) => app,
            Err(e) => return Ok(Err(crate::k8s::short_error(&e))),
        };
        let login = match super::super::server::login_for(&hub.context) {
            Ok(login) => login,
            Err(why) => {
                let note = format!("{why}: the resource tree, Argo CD's diff and the commits are read from it");
                return Ok(Ok(Read { app, tree: None, diff: None, commits: Vec::new(), note: Some(note) }));
            }
        };
        let (server, token, how) = login;
        let shown = server.name();
        let get = |path: String| {
            let (server, token) = (server.clone(), token.clone());
            async move { super::super::server::get_path(&server, &token, &path).await }
        };
        let diff_of = |v: &Value| super::super::server::diffs(v, shown.clone(), how);
        Ok(Ok(extras(&server.base, &namespace, &name, app, get, diff_of).await))
    });
    read.await.unwrap_or_else(|e| Err(format!("{e:#}")))
}
