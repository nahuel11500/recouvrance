//! The feeds read by a lookup outside the Argo CD page: an object's Application on its page,
//! an agent's `describe_resource` and `get_changes`. They read what the feeds hold, with no
//! list of their own: a feed running already (the Argo CD page's) is read as it is, and one
//! started for a lookup stays `LINGER` after the last lookup that asked for it, so a page
//! read every few seconds, or an agent's calls, list once and then follow the stream.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant};

use super::{ApiApp, Endpoint, FeedState, Watch, endpoints, lock, running, watch};

/// How long a feed started for a lookup stays after the last lookup that asked for it.
const LINGER: Duration = Duration::from_secs(5 * 60);
/// How often the feeds held for lookups are looked at, to let go of those no longer asked for.
const SWEEP_EVERY: Duration = Duration::from_secs(30);
/// How often a lookup waiting for a feed's first list looks again.
const POLL: Duration = Duration::from_millis(100);

/// The feeds held for lookups, by feed key, with when each hold ends.
static LINGERING: LazyLock<Mutex<HashMap<String, (Watch, Instant)>>> = LazyLock::new(Default::default);
/// A task lets go of the feeds no longer asked for.
static SWEEPING: AtomicBool = AtomicBool::new(false);

/// Hold the feed of `endpoint` `LINGER` from now (again, when it is held already).
fn linger(endpoint: &Endpoint) -> Arc<Mutex<FeedState>> {
    let w = watch(endpoint);
    let state = w.state.clone();
    let mut held = LINGERING.lock().unwrap_or_else(PoisonError::into_inner);
    // The hold replaced is dropped after the new one counts: the feed goes on.
    held.insert(w.key.clone(), (w, Instant::now() + LINGER));
    if !SWEEPING.swap(true, Ordering::SeqCst) {
        crate::k8s::spawn_detached(sweep());
    }
    state
}

/// Let go of the holds that ended, every `SWEEP_EVERY`, until none is left.
async fn sweep() {
    loop {
        tokio::time::sleep(SWEEP_EVERY).await;
        let mut held = LINGERING.lock().unwrap_or_else(PoisonError::into_inner);
        let now = Instant::now();
        held.retain(|_, (_, end)| *end > now);
        if held.is_empty() {
            // Under the lock: a hold added after this starts a new sweep.
            SWEEPING.store(false, Ordering::SeqCst);
            return;
        }
    }
}

/// One server's part of a lookup.
#[derive(Clone, Debug)]
pub struct Held {
    pub endpoint: Arc<Endpoint>,
    /// The Applications picked, or why the server has none to give.
    pub apps: Result<Vec<Arc<ApiApp>>, String>,
}

/// What a feed gives a lookup now: what it holds once listed (also while its stream is opened
/// again after a failure), else its error; None while its first list runs.
fn now_held(state: &FeedState, keep: impl Fn(&ApiApp) -> bool) -> Option<Result<Vec<Arc<ApiApp>>, String>> {
    if state.listed {
        let mut apps: Vec<Arc<ApiApp>> = state.apps.values().filter(|a| keep(a)).cloned().collect();
        apps.sort_by(|a, b| (&a.summary.namespace, &a.summary.name).cmp(&(&b.summary.namespace, &b.summary.name)));
        return Some(Ok(apps));
    }
    state.error.clone().map(Err)
}

/// A server to read: its feed, or why it has none.
type Source = (Arc<Endpoint>, Result<Arc<Mutex<FeedState>>, String>);

/// What each feed holds that `keep` picks, waiting at most `wait` for those still listing for
/// the first time. A feed that failed answers at once with its error: a server that is down
/// never holds the others' answers, nor the next lookup's (its feed keeps trying meanwhile).
async fn gather(sources: Vec<Source>, wait: Duration, keep: impl Fn(&Endpoint, &ApiApp) -> bool) -> Vec<Held> {
    let deadline = Instant::now() + wait;
    loop {
        let now: Vec<Option<Result<Vec<Arc<ApiApp>>, String>>> = sources
            .iter()
            .map(|(e, feed)| match feed {
                Ok(state) => now_held(&lock(state), |a| keep(e, a)),
                Err(why) => Some(Err(why.clone())),
            })
            .collect();
        if now.iter().all(Option::is_some) || Instant::now() >= deadline {
            let late = format!("did not answer within {} s", wait.as_secs());
            return sources
                .into_iter()
                .zip(now)
                .map(|((endpoint, _), apps)| Held { endpoint, apps: apps.unwrap_or_else(|| Err(late.clone())) })
                .collect();
        }
        tokio::time::sleep(POLL).await;
    }
}

/// The Applications `keep` picks on each Argo CD server turned on whose name `use_server`
/// accepts, from what its feed holds. A server whose feed isn't running is read with
/// `start` (its feed is started, and held `LINGER`), else left out. Waits at most `wait` for a
/// feed's first list; a server whose feed failed answers with its error at once. Reads the
/// servers' config (files): call it off the UI thread.
pub async fn held_apps(
    use_server: impl Fn(&str) -> bool,
    start: bool,
    wait: Duration,
    keep: impl Fn(&Endpoint, &ApiApp) -> bool,
) -> Vec<Held> {
    let mut sources = Vec::new();
    for endpoint in endpoints().list.into_iter().filter(|e| e.on && use_server(&e.name())) {
        // A login that renews is read too: its feed renews it at once.
        let feed = match &endpoint.login {
            Err(why) if !super::renews(&endpoint) => Err(why.clone()),
            _ if start => Ok(linger(&endpoint)),
            _ => match running(&endpoint) {
                Some(state) => Ok(state),
                None => continue,
            },
        };
        sources.push((Arc::new(endpoint), feed));
    }
    gather(sources, wait, keep).await
}

/// What each Argo CD server turned on whose name `use_server` accepts holds, whole (its
/// Applications, projects and clusters as its feed has them), for a reader that shows them
/// all (an agent's `argocd_apps`): its feed is started when none runs, and held `LINGER`.
/// Waits at most `wait` for a feed's first list; a server whose feed failed, or that has no
/// login, answers with why at once. Reads the servers' config (files): call it off the UI
/// thread.
pub async fn held_states(
    use_server: impl Fn(&str) -> bool,
    wait: Duration,
) -> Vec<(Arc<Endpoint>, Result<FeedState, String>)> {
    let sources: Vec<Source> = endpoints()
        .list
        .into_iter()
        .filter(|e| e.on && use_server(&e.name()))
        .map(|e| {
            let feed = match &e.login {
                Err(why) if !super::renews(&e) => Err(why.clone()),
                _ => Ok(linger(&e)),
            };
            (Arc::new(e), feed)
        })
        .collect();
    let deadline = Instant::now() + wait;
    loop {
        let now: Vec<Option<Result<FeedState, String>>> = sources
            .iter()
            .map(|(_, feed)| match feed {
                Ok(state) => {
                    let s = lock(state);
                    if s.listed { Some(Ok(s.clone())) } else { s.error.clone().map(Err) }
                }
                Err(why) => Some(Err(why.clone())),
            })
            .collect();
        if now.iter().all(Option::is_some) || Instant::now() >= deadline {
            let late = format!("did not answer within {} s", wait.as_secs());
            return sources
                .into_iter()
                .zip(now)
                .map(|((e, _), s)| (e, s.unwrap_or_else(|| Err(late.clone()))))
                .collect();
        }
        tokio::time::sleep(POLL).await;
    }
}

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};
    use std::time::{Duration, Instant};

    use serde_json::json;

    use super::{FeedState, Source, gather};
    use crate::k8s::argo::api::{ApiApp, Apps, Endpoint, Token};
    use crate::k8s::argo::server::{Login, Server};

    fn endpoint(base: &str) -> Arc<Endpoint> {
        Arc::new(Endpoint {
            server: Server::parse(base).unwrap(),
            cli_context: None,
            in_settings: true,
            insecure: false,
            login: Ok((Token("t".into()), Login::Cli)),
            on: true,
            settings_token: true,
            renewable: false,
        })
    }

    fn apps(names: &[&str]) -> Apps {
        names
            .iter()
            .map(|n| {
                let a = ApiApp::of(&json!({"metadata": {"name": n, "namespace": "argocd"}}));
                (a.key(), Arc::new(a))
            })
            .collect()
    }

    fn feed(state: FeedState) -> Result<Arc<Mutex<FeedState>>, String> {
        Ok(Arc::new(Mutex::new(state)))
    }

    /// A server that is down, or without a login, answers with why at once: with a feed that
    /// listed, they never hold the lookup. One still listing for the first time is waited for
    /// no longer than the bound, and said so.
    #[tokio::test]
    async fn a_server_down_is_never_waited_on() {
        let listed = FeedState { apps: apps(&["web", "api"]), listed: true, ..Default::default() };
        let down = FeedState { error: Some("connection refused".into()), ..Default::default() };
        // Listed before, failing now: what it holds still counts.
        let again = FeedState {
            apps: apps(&["web"]),
            listed: true,
            error: Some("stream ended".into()),
            live: false,
            ..Default::default()
        };
        let sources: Vec<Source> = vec![
            (endpoint("a.example.com"), feed(listed)),
            (endpoint("b.example.com"), feed(down)),
            (endpoint("c.example.com"), Err("no token".into())),
            (endpoint("d.example.com"), feed(again)),
        ];
        let started = Instant::now();
        let held = gather(sources, Duration::from_secs(30), |_, a| a.summary.name == "web").await;
        assert!(started.elapsed() < Duration::from_secs(1));
        let names = |i: usize| -> Vec<String> {
            held[i].apps.as_ref().unwrap().iter().map(|a| a.summary.name.clone()).collect()
        };
        assert_eq!(names(0), ["web"]);
        assert_eq!(held[1].apps.as_ref().unwrap_err(), "connection refused");
        assert_eq!(held[2].apps.as_ref().unwrap_err(), "no token");
        assert_eq!(names(3), ["web"]);

        let listing = vec![(endpoint("a.example.com"), feed(FeedState::default()))];
        let started = Instant::now();
        let held = gather(listing, Duration::from_millis(300), |_, _| true).await;
        assert!(started.elapsed() < Duration::from_secs(2));
        assert!(held[0].apps.as_ref().unwrap_err().starts_with("did not answer within"));
    }

    /// Each server's feed is kept apart: `keep` is asked with the server it is for.
    #[tokio::test]
    async fn each_server_gives_its_own() {
        let a = FeedState { apps: apps(&["web"]), listed: true, ..Default::default() };
        let b = FeedState { apps: apps(&["web", "db"]), listed: true, ..Default::default() };
        let sources: Vec<Source> = vec![(endpoint("a.example.com"), feed(a)), (endpoint("b.example.com"), feed(b))];
        let held = gather(sources, Duration::ZERO, |e, _| e.name() == "b.example.com").await;
        assert!(held[0].apps.as_ref().unwrap().is_empty());
        assert_eq!(held[1].apps.as_ref().unwrap().len(), 2);
        assert_eq!(held[1].endpoint.name(), "b.example.com");
    }
}
