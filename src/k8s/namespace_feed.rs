//! Each connected context's namespaces, kept live by one watch per context that every
//! window and page shares: a namespace created or deleted after the connect shows (or goes)
//! in the namespace picker without connecting again. The watch is a table watch
//! (`watch_table_paused`), with its relists, reconnects and backoff; it runs while one
//! `NamespaceHold` on the context is held.

use std::collections::HashMap;
use std::sync::{Arc, LazyLock, Mutex, PoisonError};

use futures::StreamExt as _;
use futures::channel::mpsc::UnboundedReceiver;
use k8s_openapi::api::core::v1::Namespace;
use kube::Client;
use tokio::sync::watch;

use super::{TableMsg, TaskGuard};

/// What a context's namespace watch answered so far.
#[derive(Clone, Debug, Default, PartialEq)]
pub enum Answer {
    /// Not yet: its first list is on the way.
    #[default]
    Waiting,
    /// Listed: `names` holds every namespace, and follows the watch.
    Listed,
    /// The user may not list namespaces there (403): they are picked by hand. The watch stops.
    Forbidden,
    /// The last list or watch failed, with why. `names` keeps what was listed before, if
    /// anything was; the watch tries again.
    Failed(String),
}

/// A context's namespaces as its watch sees them.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Namespaces {
    /// Sorted, each once.
    pub names: Arc<Vec<String>>,
    pub answer: Answer,
    /// Listed once at least: `names` is a whole list (it may be older than a failure since).
    pub listed: bool,
}

impl Namespaces {
    /// Whether the watch answered once: listed, refused or failed.
    pub fn answered(&self) -> bool {
        self.answer != Answer::Waiting
    }

    /// The namespaces as a connect takes them: listed, none when listing them is refused (they
    /// are picked by hand), or why the listing failed.
    pub fn result(&self) -> Result<Vec<String>, String> {
        match &self.answer {
            Answer::Forbidden => Ok(Vec::new()),
            Answer::Failed(e) if !self.listed => Err(e.clone()),
            _ => Ok(self.names.to_vec()),
        }
    }

    /// Apply what the watch sent; whether anything changed. A `Rows` holds every namespace (the
    /// table watch sends whole snapshots once listed, and the first list whole: see `start`).
    pub fn apply(&mut self, msg: TableMsg) -> bool {
        let before = self.clone();
        match msg {
            TableMsg::Rows(rows) => {
                let mut names: Vec<String> = rows.iter().map(|r| r.name.clone()).collect();
                names.sort();
                names.dedup();
                if *self.names != names {
                    self.names = Arc::new(names);
                }
                // Rows after an error: the watch is back.
                if matches!(self.answer, Answer::Failed(_)) && self.listed {
                    self.answer = Answer::Listed;
                }
            }
            TableMsg::Listed | TableMsg::AllRows => {
                self.listed = true;
                self.answer = Answer::Listed;
            }
            TableMsg::Forbidden(_) => {
                self.answer = Answer::Forbidden;
                self.names = Arc::default();
                self.listed = false;
            }
            TableMsg::Error(e) => self.answer = Answer::Failed(e),
            TableMsg::Columns(_) => {}
        }
        *self != before
    }
}

/// A context's watch while someone holds it.
struct Feed {
    state: watch::Sender<Namespaces>,
    users: usize,
    _task: TaskGuard,
}

static FEEDS: LazyLock<Mutex<HashMap<String, Feed>>> = LazyLock::new(Default::default);
/// Counts every change of any context's namespaces: windows read their contexts' again.
static CHANGES: LazyLock<watch::Sender<u64>> = LazyLock::new(|| watch::channel(0).0);

fn feeds() -> std::sync::MutexGuard<'static, HashMap<String, Feed>> {
    FEEDS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Wakes whenever any context's namespaces change.
pub fn changes() -> watch::Receiver<u64> {
    CHANGES.subscribe()
}

/// A hold on a context's namespace watch: it runs while one is held, shared by every holder.
pub struct NamespaceHold {
    context: String,
    state: watch::Receiver<Namespaces>,
}

impl NamespaceHold {
    /// The namespaces as the watch sees them now.
    pub fn now(&self) -> Namespaces {
        self.state.borrow().clone()
    }

    /// The watch's first answer (at once when another holder's watch already has one).
    pub async fn answer(&mut self) -> Namespaces {
        if let Ok(n) = self.state.wait_for(Namespaces::answered).await {
            return n.clone();
        }
        // Its watch is gone (it never is while held): what it last said.
        self.state.borrow().clone()
    }

    pub fn context(&self) -> &str {
        &self.context
    }
}

impl Drop for NamespaceHold {
    fn drop(&mut self) {
        let mut feeds = feeds();
        if let Some(f) = feeds.get_mut(&self.context)
            && self.state.same_channel(&f.state.subscribe())
        {
            f.users = f.users.saturating_sub(1);
            if f.users == 0 {
                // Dropping it stops its watch.
                feeds.remove(&self.context);
            }
        }
    }
}

/// Hold `context`'s namespace watch, started with `client` unless one runs already. One that
/// was refused or is failing starts again with `client`: a new connect may bring new rights
/// or credentials.
pub fn hold(context: &str, client: &Client) -> NamespaceHold {
    let client = client.clone();
    hold_with(context, move || {
        let kind = super::kind_of(super::ar_of::<Namespace>(), false);
        super::watch_table_paused(client, kind, None, Arc::default(), super::Pause::default(), false, None)
    })
}

/// `hold` with the watch `start` begins (a fake one in tests).
fn hold_with(context: &str, start: impl FnOnce() -> (TaskGuard, UnboundedReceiver<TableMsg>)) -> NamespaceHold {
    let mut feeds = feeds();
    let feed = match feeds.entry(context.to_string()) {
        std::collections::hash_map::Entry::Occupied(e) => {
            let f = e.into_mut();
            // Refused or failing: watched again, on the same channel so that its holders
            // follow the new watch. Its names stay until the new list replaces them.
            if matches!(f.state.borrow().answer, Answer::Forbidden | Answer::Failed(_)) {
                f.state.send_modify(|n| n.answer = Answer::Waiting);
                f._task = pump(f.state.clone(), start);
            }
            f
        }
        std::collections::hash_map::Entry::Vacant(e) => {
            let state = watch::channel(Namespaces::default()).0;
            let task = pump(state.clone(), start);
            e.insert(Feed { state, users: 0, _task: task })
        }
    };
    feed.users += 1;
    NamespaceHold { context: context.to_string(), state: feed.state.subscribe() }
}

/// Start the watch and apply what it sends to `state`, until the guard returned is dropped.
fn pump(
    state: watch::Sender<Namespaces>,
    start: impl FnOnce() -> (TaskGuard, UnboundedReceiver<TableMsg>),
) -> TaskGuard {
    let (guard, mut msgs) = start();
    super::spawn_guarded(async move {
        // Dropped with this task: the watch stops with it.
        let _watch = guard;
        while let Some(msg) = msgs.next().await {
            if state.send_if_modified(|n| n.apply(msg)) {
                CHANGES.send_modify(|c| *c += 1);
            }
        }
    })
}

/// `context`'s namespaces, when a watch holds a whole list of them: what pages that count or
/// list namespaces read instead of listing them again.
pub fn listed(context: &str) -> Option<Arc<Vec<String>>> {
    let feeds = feeds();
    let state = feeds.get(context)?.state.borrow();
    (state.listed && state.answer == Answer::Listed).then(|| state.names.clone())
}

#[cfg(test)]
mod tests {
    use futures::channel::mpsc;

    use super::*;
    use crate::k8s::Row;

    fn row(name: &str) -> Arc<Row> {
        Arc::new(Row {
            uid: format!("uid-{name}"),
            name: name.into(),
            namespace: None,
            created: None,
            cells: Vec::new(),
            labels: "".into(),
            deleting: false,
            deleted_at: None,
        })
    }

    fn rows(names: &[&str]) -> TableMsg {
        TableMsg::Rows(names.iter().map(|n| row(n)).collect())
    }

    #[test]
    fn namespaces_created_and_deleted_after_the_list_follow_the_watch() {
        let mut n = Namespaces::default();
        assert!(!n.answered());
        n.apply(rows(&["kube-system", "default"]));
        n.apply(TableMsg::Listed);
        assert_eq!(n.result(), Ok(vec!["default".to_string(), "kube-system".to_string()]));
        // One created: the next snapshot has it.
        assert!(n.apply(rows(&["default", "kube-system", "team-a"])));
        assert_eq!(*n.names, ["default", "kube-system", "team-a"]);
        // One deleted: gone.
        assert!(n.apply(rows(&["default", "team-a"])));
        assert_eq!(*n.names, ["default", "team-a"]);
        // The same snapshot again changes nothing (no window wakes for it).
        assert!(!n.apply(rows(&["team-a", "default"])));
    }

    #[test]
    fn a_refusal_is_an_empty_answer_and_a_failure_keeps_what_was_listed() {
        let mut n = Namespaces::default();
        n.apply(TableMsg::Forbidden("forbidden".into()));
        assert!(n.answered());
        assert_eq!(n.result(), Ok(Vec::new()));

        // Failing before any list: the connect reports it.
        let mut n = Namespaces::default();
        n.apply(TableMsg::Error("timed out".into()));
        assert_eq!(n.result(), Err("timed out".to_string()));
        assert_eq!(listed_of(&n), None);
        // Listed later: the names come, the failure goes.
        n.apply(rows(&["a"]));
        n.apply(TableMsg::Listed);
        assert_eq!(n.result(), Ok(vec!["a".to_string()]));

        // A watch cut after the list keeps its names.
        n.apply(TableMsg::Error("connection reset".into()));
        assert_eq!(n.result(), Ok(vec!["a".to_string()]));
        n.apply(rows(&["a", "b"]));
        assert_eq!(n.answer, Answer::Listed);
    }

    fn listed_of(n: &Namespaces) -> Option<Vec<String>> {
        (n.listed && n.answer == Answer::Listed).then(|| n.names.to_vec())
    }

    /// A fake watch: what the test sends is what the feed gets.
    fn fake(
        started: &Arc<Mutex<usize>>,
    ) -> (mpsc::UnboundedSender<TableMsg>, impl FnOnce() -> (TaskGuard, UnboundedReceiver<TableMsg>)) {
        let (tx, rx) = mpsc::unbounded();
        let started = started.clone();
        (tx, move || {
            *started.lock().unwrap() += 1;
            (crate::k8s::spawn_guarded(std::future::pending()), rx)
        })
    }

    #[test]
    fn every_holder_of_a_context_shares_one_watch_until_the_last_one_goes() {
        let ctx = "namespace-feed-test-shared";
        let started = Arc::new(Mutex::new(0));
        let (tx, start) = fake(&started);
        let mut first = hold_with(ctx, start);
        let (_unused, start_again) = fake(&started);
        let mut second = hold_with(ctx, start_again);
        assert_eq!(*started.lock().unwrap(), 1, "one watch per context");

        tx.unbounded_send(rows(&["default"])).unwrap();
        tx.unbounded_send(TableMsg::Listed).unwrap();
        let answered = futures::executor::block_on(first.answer());
        assert_eq!(*answered.names, ["default"]);
        assert_eq!(*futures::executor::block_on(second.answer()).names, ["default"]);

        // A namespace created later reaches both holders, and wakes the windows.
        let mut woken = changes();
        woken.mark_unchanged();
        tx.unbounded_send(rows(&["default", "new"])).unwrap();
        futures::executor::block_on(async {
            woken.changed().await.unwrap();
            second.state.wait_for(|n| n.names.len() == 2).await.unwrap();
        });
        assert_eq!(*second.now().names, ["default", "new"]);
        assert_eq!(listed(ctx).as_deref().map(Vec::as_slice), Some(&["default".to_string(), "new".to_string()][..]));

        drop(first);
        assert!(listed(ctx).is_some(), "still held by the second");
        drop(second);
        assert!(listed(ctx).is_none(), "the last hold stops the watch");
    }

    #[test]
    fn a_refused_watch_starts_again_on_the_next_hold() {
        let ctx = "namespace-feed-test-refused";
        let started = Arc::new(Mutex::new(0));
        let (tx, start) = fake(&started);
        let mut first = hold_with(ctx, start);
        tx.unbounded_send(TableMsg::Forbidden("forbidden".into())).unwrap();
        assert_eq!(futures::executor::block_on(first.answer()).answer, Answer::Forbidden);
        // A new connect (new rights): a new watch, shared from then on.
        let (tx2, start2) = fake(&started);
        let mut second = hold_with(ctx, start2);
        assert_eq!(*started.lock().unwrap(), 2);
        tx2.unbounded_send(rows(&["team-a"])).unwrap();
        tx2.unbounded_send(TableMsg::Listed).unwrap();
        assert_eq!(*futures::executor::block_on(second.answer()).names, ["team-a"]);
        // The first hold counts on the new watch: dropping it alone doesn't stop it.
        drop(first);
        assert!(listed(ctx).is_some());
        drop(second);
        assert!(listed(ctx).is_none());
    }
}
