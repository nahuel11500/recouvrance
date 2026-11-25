//! Reading a dashboard: the Application, then its tree, its diff and its commits.

use std::collections::HashMap;
use std::future::Future;
use std::sync::{LazyLock, Mutex, PoisonError};

use serde_json::Value;

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
    let diff = Some(managed.as_ref().map(&diff_of).map_err(Clone::clone));
    Read { app, tree: Some(tree), diff, managed: managed.ok(), commits, note: None }
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
